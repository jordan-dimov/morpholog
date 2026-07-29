//! Morpholog v0 semantic kernel.
//!
//! The synchronous, pure heart of Morpholog. It defines the IR
//! (invariants, transformations, claims, statements, expressions),
//! evaluates invariants against in-memory state, and exposes [`propose`],
//! the function that turns a proposed transformation into either an
//! accepted post-state or a rejected attempt.
//!
//! Does no I/O. The PostgreSQL persistence adapter lives in the separate
//! `morpholog-postgres` crate and wraps this kernel as an async boundary;
//! async must not infect this crate. Worked-example IR lives in the
//! `morpholog-examples` crate.
//!
//! The kernel is the trust boundary: its evaluation and proposal paths
//! reject malformed input with a typed `EvalError`, never a `panic!`, and
//! it never touches floating point (business values are decimal). The
//! `warn`s below keep both mechanical - `panic!` stays out of non-test
//! code and float arithmetic is a compile error. (Internal guards still
//! `assert!` / `unreachable!` on structurally-impossible IR; those are
//! programmer-error checks on already-validated data, not the input path,
//! and `clippy::panic` covers neither.) Test code is exempt via
//! `.clippy.toml` (`allow-panic-in-tests`).
#![warn(clippy::panic, clippy::float_arithmetic)]

pub mod actor_repr;
pub mod format;
pub mod ir_builder;

pub mod analysis;
mod check;
mod compiled;
mod controls;
mod coverage;
mod definitions;
mod derive;
mod disciplines;
mod eval;
mod explain;
mod fold;
mod guarantees;
mod ir;
mod lint;
mod propose;
pub mod schema;
mod score;
mod state;
mod sums;
mod validate;

pub use analysis::{
    AnalysisError, ParamKind, predicates_read_by_stmt, predicates_referenced_by_derived,
    predicates_referenced_by_prop, transformation_param_kinds, transformations_asserting,
};
pub use compiled::CompiledProgram;
pub use controls::{
    ControlMatrix, GateControl, GateFrontLoad, GateRef, InvariantFrontLoad, TransformationControls,
    controls, render_controls,
};
pub use coverage::{
    CoverageReport, CoverageTracker, CoverageVerdict, InvariantCoverage, TransformationUsage,
    render_coverage,
};
pub use definitions::resolve_defined_calls;
pub use derive::{enumerate_derived, eval_invariant, invariant_witness};
pub use disciplines::{in_force_define_name, lower_discipline_definitions, lower_disciplines};
pub use eval::{EvalError, RenderedClaim};
pub use explain::{
    ErrorRejection, Explanation, GateKind, GateRejection, InvariantRejection, MissingClaim,
    Rejection, TransitionRef, Verdict, explain,
};
pub use guarantees::{Guarantee, guarantees, render_guarantees};
pub use ir::{
    ArgDecl, ArithOp, Claim, CompareOp, Definition, DefinitionName, DefinitionOrigin, DerivedClaim,
    DerivedValue, Discipline, ExtremumOp, Intent, IntentDecl, IntentName, Invariant, InvariantName,
    InvariantOrigin, OrderedDomain, PredicateArgKind, PredicateDecl, PredicateName, Program, Prop,
    RuleName, Stmt, Subject, SumSeed, Term, Transformation, TransformationName, Unit, Value,
    ValueExpr, Var,
};
pub use lint::{Lint, lints};
pub use propose::{
    BindOneOutcome, ForIterationTrace, Outcome, RejectionReason, RequireOutcome, StagedDelta,
    TraceEntry, TracedProposal, Transition, WitnessBinding, propose, propose_stage_delta,
    propose_with_trace,
};
pub use schema::{intent_arg_schema, transformation_arg_schema};
pub use score::{
    BatchScore, CandidateScore, CandidateScorer, CaseOutcome, CaseResult, InvariantScore,
    SCORE_FORMAT_VERSION, SCORE_SEMANTICS, ScoreError, SliceInvariantScore, SliceScore,
    SplitBoundaryReport, SplitScore, invariants_using_pre,
};
pub use state::{ClaimInstance, EvalValue, IntentInstance, State};
pub use sums::lower_sum_seeds;
pub use validate::{ValidatedProgram, ValidationContext, ValidationError, VocabularyKind};

#[cfg(test)]
mod tests {
    //! Kernel-internal unit tests that depend on private items
    //! (`unify_args`, `resolve_term`, `Bindings`). Tests against the
    //! public surface live in `tests/` as integration tests.

    use super::*;
    use crate::eval::{EvalContext, eval_value, find_matches, resolve_term, unify_args};

    // Boxed-operand builders keep comparator-heavy test call
    // sites terse. Operands
    // are value expressions; the comparison itself is a proposition.
    fn le_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
        Prop::Compare {
            op: CompareOp::Le,
            domain: OrderedDomain::Decimal,
            left: l,
            right: r,
        }
    }
    fn lt_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
        Prop::Compare {
            op: CompareOp::Lt,
            domain: OrderedDomain::Decimal,
            left: l,
            right: r,
        }
    }
    fn ge_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
        Prop::Compare {
            op: CompareOp::Ge,
            domain: OrderedDomain::Decimal,
            left: l,
            right: r,
        }
    }
    fn gt_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
        Prop::Compare {
            op: CompareOp::Gt,
            domain: OrderedDomain::Decimal,
            left: l,
            right: r,
        }
    }
    fn date_le_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
        Prop::Compare {
            op: CompareOp::Le,
            domain: OrderedDomain::Date,
            left: l,
            right: r,
        }
    }
    fn date_lt_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
        Prop::Compare {
            op: CompareOp::Lt,
            domain: OrderedDomain::Date,
            left: l,
            right: r,
        }
    }
    fn date_ge_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
        Prop::Compare {
            op: CompareOp::Ge,
            domain: OrderedDomain::Date,
            left: l,
            right: r,
        }
    }
    fn date_gt_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
        Prop::Compare {
            op: CompareOp::Gt,
            domain: OrderedDomain::Date,
            left: l,
            right: r,
        }
    }
    use crate::state::Bindings;
    use jiff::civil::Date;
    use rust_decimal::Decimal;
    use std::collections::BTreeSet;

    /// No-actor, no-pre EvalContext for standalone expression evaluation.
    fn ctx<'a>(state: &'a State, bindings: &'a Bindings) -> EvalContext<'a> {
        EvalContext::new(
            state,
            None,
            bindings,
            None,
            crate::definitions::DefinitionIndex::new(&[]),
        )
    }

    /// No-actor EvalContext with both pre and post states, for `Prop::Pre`
    /// tests where the wrapped subtree flips into pre-state lookup.
    fn ctx_with_pre<'a>(
        state: &'a State,
        pre: &'a State,
        bindings: &'a Bindings,
    ) -> EvalContext<'a> {
        EvalContext::new(
            state,
            Some(pre),
            bindings,
            None,
            crate::definitions::DefinitionIndex::new(&[]),
        )
    }

    #[test]
    fn decimal_literal_constructs() {
        let v = Value::Decimal("1250.75".to_string());
        assert_eq!(
            Term::Literal(v),
            Term::Literal(Value::Decimal("1250.75".to_string()))
        );
    }

    #[test]
    fn subject_literal_constructs_and_resolves() {
        let v = Value::Subject("bank_debt_service".into());
        assert_eq!(
            Term::Literal(v),
            Term::Literal(Value::Subject("bank_debt_service".into()))
        );
        let resolved = resolve_term(
            &Term::Literal(Value::Subject("bank_debt_service".into())),
            &Bindings::new(),
            None,
        )
        .unwrap();
        assert_eq!(resolved, EvalValue::Subject("bank_debt_service".into()));
    }

    #[test]
    fn subject_literal_unifies_with_matching_subject_arg() {
        let pattern = vec![Term::Literal(Value::Subject("p1".into()))];
        let value = vec![EvalValue::Subject("p1".into())];
        assert!(unify_args(&pattern, &value, &Bindings::new(), None).is_some());

        let mismatch = vec![EvalValue::Subject("p2".into())];
        assert!(unify_args(&pattern, &mismatch, &Bindings::new(), None).is_none());

        let wrong_kind = vec![EvalValue::Decimal(Decimal::new(1, 0))];
        assert!(unify_args(&pattern, &wrong_kind, &Bindings::new(), None).is_none());
    }

    /// Pins the contract of `State::claims_for`: it returns *only*
    /// claims whose predicate matches the requested name, it returns
    /// them with arg values intact, it returns an empty iterator for
    /// predicates that have no admitted claims, and it does not
    /// interfere with `State::claims` returning the construction-order
    /// list.
    #[test]
    fn claims_for_returns_only_matching_predicate() {
        let a1 = ClaimInstance {
            predicate: "A".into(),
            args: vec![EvalValue::Subject("a1".into())],
        };
        let b1 = ClaimInstance {
            predicate: "B".into(),
            args: vec![EvalValue::Decimal(Decimal::new(42, 0))],
        };
        let a2 = ClaimInstance {
            predicate: "A".into(),
            args: vec![EvalValue::Subject("a2".into())],
        };
        let state = State::from_claims(vec![a1.clone(), b1.clone(), a2.clone()]);

        let a_rows: Vec<&ClaimInstance> = state.claims_for("A").collect();
        assert_eq!(a_rows.len(), 2, "two A claims admitted");
        assert!(a_rows.iter().all(|c| c.predicate.as_str() == "A"));
        assert!(a_rows.contains(&&a1));
        assert!(a_rows.contains(&&a2));

        let b_rows: Vec<&ClaimInstance> = state.claims_for("B").collect();
        assert_eq!(b_rows, vec![&b1], "single B claim, args intact");

        let absent: Vec<&ClaimInstance> = state.claims_for("Nope").collect();
        assert!(
            absent.is_empty(),
            "no claims for an unknown predicate, not an error"
        );

        assert_eq!(
            state.claims(),
            &[a1, b1, a2],
            "claims() preserves construction order across all predicates"
        );
    }

    /// Pins the contract of `State::claim_indices_for_arg`: it returns
    /// the indices of claims with the requested predicate where the
    /// argument at the requested position equals the requested value,
    /// `None` (not Some empty) when no such bucket exists, and does
    /// not match claims of a different predicate that happen to share
    /// a value at the same position. The lookup is what
    /// `find_claim_matches` uses to make ground-argument matching
    /// O(bucket size) instead of O(predicate size).
    #[test]
    fn claim_indices_for_arg_narrows_by_predicate_position_and_value() {
        let line_for_entry_a = ClaimInstance {
            predicate: "JournalLine".into(),
            args: vec![
                EvalValue::Subject("entry_a".into()),
                EvalValue::Subject("account_cash".into()),
            ],
        };
        let line_for_entry_b = ClaimInstance {
            predicate: "JournalLine".into(),
            args: vec![
                EvalValue::Subject("entry_b".into()),
                EvalValue::Subject("account_cash".into()),
            ],
        };
        // Same value at position 0 but different predicate; must not
        // pollute the JournalLine[0=entry_a] bucket.
        let je_for_entry_a = ClaimInstance {
            predicate: "JournalEntry".into(),
            args: vec![EvalValue::Subject("entry_a".into())],
        };
        let state = State::from_claims(vec![
            line_for_entry_a.clone(),
            line_for_entry_b.clone(),
            je_for_entry_a.clone(),
        ]);

        let entry_a = EvalValue::Subject("entry_a".into());
        let positions = state
            .claim_indices_for_arg(&"JournalLine".into(), 0, &entry_a)
            .expect("entry_a appears at JournalLine[0]");
        let claims: Vec<&ClaimInstance> = positions.iter().map(|&i| state.claim_at(i)).collect();
        assert_eq!(
            claims,
            vec![&line_for_entry_a],
            "must return only the JournalLine claim, not JournalEntry"
        );

        let unknown = EvalValue::Subject("entry_z".into());
        assert!(
            state
                .claim_indices_for_arg(&"JournalLine".into(), 0, &unknown)
                .is_none(),
            "absent value returns None, signalling empty intersection"
        );

        let cash = EvalValue::Subject("account_cash".into());
        let cash_positions = state
            .claim_indices_for_arg(&"JournalLine".into(), 1, &cash)
            .expect("account_cash appears at JournalLine[1]");
        assert_eq!(
            cash_positions.len(),
            2,
            "both JournalLine claims share account_cash at position 1"
        );
    }

    /// Pins `predicates_referenced_by_prop`: a `Prop` touching every
    /// variant that carries a nested `Prop` or `Claim` node, each site
    /// using a unique predicate name, must extract every planted name.
    /// Comparator operands are value expressions, so the planted names
    /// at those positions arrive via `Sum`/`ValueOf` (the value walk).
    #[test]
    fn predicates_referenced_by_prop_covers_every_variant() {
        let claim = |p: &str| Prop::Claim {
            predicate: p.into(),
            args: vec![],
        };
        // A value expression that plants one predicate name (a `Sum`
        // whose body is a claim), to reach the comparator-operand path.
        let value_with = |p: &str| ValueExpr::Sum {
            value: Term::Var("v".into()),
            body: Box::new(claim(p)),
            seed: SumSeed::default(),
        };

        let prop = Prop::And(vec![
            Prop::Implies {
                left: Box::new(claim("P_implies_left")),
                right: Box::new(claim("P_implies_right")),
            },
            Prop::Exists {
                binding: "x".into(),
                body: Box::new(claim("P_exists_body")),
            },
            Prop::Not(Box::new(claim("P_not_body"))),
            Prop::Eq(
                Box::new(value_with("P_eq_left")),
                Box::new(value_with("P_eq_right")),
            ),
            le_(
                Box::new(value_with("P_le_left")),
                Box::new(value_with("P_le_right")),
            ),
            date_le_(
                Box::new(value_with("P_datele_left")),
                Box::new(value_with("P_datele_right")),
            ),
            Prop::Forall {
                binding: "y".into(),
                source: Box::new(claim("P_forall_source")),
                body: Box::new(claim("P_forall_body")),
            },
            Prop::Or(vec![claim("P_or_left"), claim("P_or_right")]),
            Prop::Pre(Box::new(claim("P_pre_inner"))),
            // Variants carrying no predicate references: must contribute
            // nothing. The exhaustive set comparison below would catch
            // any spurious entry.
            Prop::Neq(
                Box::new(ValueExpr::Term(Term::Var("a".into()))),
                Box::new(ValueExpr::Term(Term::Var("b".into()))),
            ),
            Prop::In(Term::Var("e".into()), Term::Var("coll".into())),
        ]);

        let mut got = BTreeSet::new();
        predicates_referenced_by_prop(&prop, &[], &mut got);

        let expected: BTreeSet<PredicateName> = [
            "P_implies_left",
            "P_implies_right",
            "P_exists_body",
            "P_not_body",
            "P_eq_left",
            "P_eq_right",
            "P_le_left",
            "P_le_right",
            "P_datele_left",
            "P_datele_right",
            "P_forall_source",
            "P_forall_body",
            "P_or_left",
            "P_or_right",
            "P_pre_inner",
        ]
        .iter()
        .map(|s| PredicateName::from(*s))
        .collect();

        assert_eq!(
            got, expected,
            "every Prop variant that carries a predicate reference must contribute it"
        );
    }

    /// Pins `predicates_referenced_by_value`: a `ValueExpr` touching
    /// every variant that carries a nested predicate reference (a
    /// `ValueOf`, a `Sum` body, an arithmetic operand subtree) must
    /// extract every planted name. `Term` carries none.
    #[test]
    fn predicates_referenced_by_value_covers_every_variant() {
        let claim = |p: &str| Prop::Claim {
            predicate: p.into(),
            args: vec![],
        };
        let value_of = |p: &str, default: Option<ValueExpr>| ValueExpr::ValueOf {
            predicate: p.into(),
            args: vec![Term::Wildcard],
            default: default.map(Box::new),
        };

        let expr = ValueExpr::Arith {
            op: ArithOp::Add,
            left: Box::new(ValueExpr::Arith {
                op: ArithOp::Sub,
                left: Box::new(ValueExpr::Sum {
                    value: Term::Var("v".into()),
                    body: Box::new(claim("P_sum_body")),
                    seed: SumSeed::default(),
                }),
                right: Box::new(value_of(
                    "P_valueof_self",
                    Some(value_of("P_valueof_default", None)),
                )),
            }),
            // A bare term carries no predicate reference.
            right: Box::new(ValueExpr::Term(Term::Var("z".into()))),
        };

        let mut got = BTreeSet::new();
        analysis::predicates_referenced_by_value(&expr, &[], &mut got);

        let expected: BTreeSet<PredicateName> =
            ["P_sum_body", "P_valueof_self", "P_valueof_default"]
                .iter()
                .map(|s| PredicateName::from(*s))
                .collect();

        assert_eq!(
            got, expected,
            "every ValueExpr variant that carries a predicate reference must contribute it"
        );
    }

    /// `predicates_read_by_stmt` includes every predicate the
    /// statement reads from pre-state (Require, BindOne, Let value,
    /// For collection + body, Retract pattern) and excludes
    /// `Stmt::Assert`'s output predicate.
    #[test]
    fn predicates_read_by_stmt_excludes_assert_includes_retract_and_reads() {
        use ir_builder::*;
        let body = vec![
            require(claim("P_require", vec![var("x")])),
            bind_one(claim("P_bind", vec![var("y"), var("z")])),
            let_("v", value_of("P_let", vec![var("y"), wildcard()])),
            // Writes only: P_assert MUST NOT appear in the read set.
            assert_("P_assert", vec![var("y")]),
            retract("P_retract", vec![wildcard()]),
            for_(
                "i",
                term(var("xs")),
                vec![require(claim("P_for_inner", vec![var("i")]))],
            ),
            emit("Notified", vec![var("y")]),
        ];
        let mut got = BTreeSet::new();
        for stmt in &body {
            predicates_read_by_stmt(stmt, &[], &mut got);
        }
        let expected: BTreeSet<PredicateName> =
            ["P_require", "P_bind", "P_let", "P_retract", "P_for_inner"]
                .iter()
                .map(|s| PredicateName::from(*s))
                .collect();
        assert_eq!(
            got, expected,
            "read-set must include every pre-state read and exclude Stmt::Assert's output"
        );
    }

    /// `ArithOp::Add` returns the decimal sum of its operands when both
    /// evaluate to decimals.
    #[test]
    fn add_sums_two_decimals() {
        let expr = ValueExpr::Arith {
            op: ArithOp::Add,
            left: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
                "10".to_string(),
            )))),
            right: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
                "32.5".to_string(),
            )))),
        };
        let v = eval_value(&expr, &ctx(&State::from_claims(vec![]), &Bindings::new())).unwrap();
        assert_eq!(v, EvalValue::Decimal(Decimal::new(425, 1)));
    }

    /// Non-decimal operands surface as `TypeMismatch` rather than
    /// falling through silently. Same contract as `Sub`.
    #[test]
    fn add_with_non_decimal_operand_is_type_mismatch() {
        let expr = ValueExpr::Arith {
            op: ArithOp::Add,
            left: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
                "10".to_string(),
            )))),
            right: Box::new(ValueExpr::Term(Term::Literal(Value::Subject(
                "oops".into(),
            )))),
        };
        let err = eval_value(&expr, &ctx(&State::from_claims(vec![]), &Bindings::new()))
            .expect_err("expected TypeMismatch");
        match err {
            EvalError::TypeMismatch(msg) => assert!(msg.contains("Add")),
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    fn date_lit(s: &str) -> ValueExpr {
        ValueExpr::Term(Term::Literal(Value::Date(s.to_string())))
    }

    /// `DateLe(a, b)` admits `a <= b` and returns the unchanged
    /// binding set, mirroring decimal `Le`; `a > b` is the lawful
    /// no-match path, distinct from `TypeMismatch`. Equal dates
    /// admitting pins the **inclusive** validity-window semantics -
    /// `effective_to == action_date` is admissible, not rejected.
    #[test]
    fn date_le_pins_direction_and_inclusivity() {
        let cases = [
            ("2026-03-11", "2026-03-12", 1), // earlier admits
            ("2026-03-12", "2026-03-12", 1), // equal admits: inclusive window
            ("2026-03-13", "2026-03-12", 0), // later is a lawful no-match
        ];
        for (lhs, rhs, expected) in cases {
            let expr = date_le_(Box::new(date_lit(lhs)), Box::new(date_lit(rhs)));
            let matches =
                find_matches(&expr, &ctx(&State::from_claims(vec![]), &Bindings::new())).unwrap();
            assert_eq!(matches.len(), expected, "DateLe({lhs}, {rhs})");
        }
    }

    /// Mixed operand kinds raise `TypeMismatch`, not silent rejection,
    /// and the type guard covers both positions. A malformed
    /// `Value::Date` source string surfaces the same way at
    /// evaluation time, mirroring an invalid `Value::Decimal`: there
    /// is no separate IR validation pass; parsing is the evaluator's
    /// concern.
    #[test]
    fn date_le_operand_type_guards_raise_type_mismatch() {
        let dec_lit = ValueExpr::Term(Term::Literal(Value::Decimal("1".to_string())));
        let subj_lit = ValueExpr::Term(Term::Literal(Value::Subject("oops".into())));
        let cases = [
            (dec_lit, date_lit("2026-03-12"), "civil-date"),
            (date_lit("2026-03-12"), subj_lit, "civil-date"),
            (
                date_lit("not-a-date"),
                date_lit("2026-03-12"),
                "invalid civil date",
            ),
        ];
        for (lhs, rhs, fragment) in cases {
            let expr = date_le_(Box::new(lhs), Box::new(rhs));
            let err = find_matches(&expr, &ctx(&State::from_claims(vec![]), &Bindings::new()))
                .expect_err("mixed or malformed operands must be a TypeMismatch");
            match err {
                EvalError::TypeMismatch(msg) => {
                    assert!(msg.contains(fragment), "msg was: {msg}")
                }
                other => panic!("expected TypeMismatch, got {other:?}"),
            }
        }
    }

    /// The decimal strict/inclusive comparators admit in the right
    /// direction: `Gt`/`Lt` are strict, `Ge` includes equality.
    #[test]
    fn decimal_strict_comparators_pin_direction() {
        let d = |s: &str| {
            Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
                s.to_string(),
            ))))
        };
        let admits = |e: Prop| {
            !find_matches(&e, &ctx(&State::from_claims(vec![]), &Bindings::new()))
                .unwrap()
                .is_empty()
        };
        assert!(admits(gt_(d("5"), d("3"))));
        assert!(!admits(gt_(d("3"), d("5"))));
        assert!(!admits(gt_(d("3"), d("3"))));
        assert!(admits(lt_(d("3"), d("5"))));
        assert!(!admits(lt_(d("3"), d("3"))));
        assert!(admits(ge_(d("3"), d("3"))));
        assert!(!admits(ge_(d("3"), d("5"))));
    }

    /// The civil-date comparators mirror the decimal ones: `before`
    /// (`DateLt`) and `after` (`DateGt`) are strict, `on_or_after`
    /// (`DateGe`) includes equality.
    #[test]
    fn date_strict_comparators_pin_direction() {
        let admits = |e: Prop| {
            !find_matches(&e, &ctx(&State::from_claims(vec![]), &Bindings::new()))
                .unwrap()
                .is_empty()
        };
        assert!(admits(date_lt_(
            Box::new(date_lit("2026-01-01")),
            Box::new(date_lit("2026-06-01")),
        )));
        assert!(!admits(date_lt_(
            Box::new(date_lit("2026-06-01")),
            Box::new(date_lit("2026-06-01")),
        )));
        assert!(admits(date_gt_(
            Box::new(date_lit("2026-06-01")),
            Box::new(date_lit("2026-01-01")),
        )));
        assert!(admits(date_ge_(
            Box::new(date_lit("2026-06-01")),
            Box::new(date_lit("2026-06-01")),
        )));
    }

    /// A `Value::Date` literal in a `claim` argument matches a claim
    /// admitted with the same date in that position. Pins the
    /// unify-against-literal-date path.
    #[test]
    fn date_literal_unifies_with_matching_date_arg() {
        let claim = ClaimInstance {
            predicate: "OnDate".into(),
            args: vec![EvalValue::Date(
                "2026-03-12".parse::<Date>().expect("hand-built ISO date"),
            )],
        };
        let state = State::from_claims(vec![claim]);
        let expr = Prop::Claim {
            predicate: "OnDate".into(),
            args: vec![Term::Literal(Value::Date("2026-03-12".to_string()))],
        };
        let matches = find_matches(&expr, &ctx(&state, &Bindings::new())).unwrap();
        assert_eq!(matches.len(), 1, "literal date arg must unify");

        let other = Prop::Claim {
            predicate: "OnDate".into(),
            args: vec![Term::Literal(Value::Date("2026-03-13".to_string()))],
        };
        let none = find_matches(&other, &ctx(&state, &Bindings::new())).unwrap();
        assert!(
            none.is_empty(),
            "literal date arg must not unify with a different date"
        );
    }

    /// The cumulative-cap shape: an `Arith` addition nested under a `<=`
    /// comparison (`running + proposed <= cap`), gating an authorisation
    /// under an aggregate limit. Pins the composition so the kernel cannot
    /// drift.
    #[test]
    fn add_nests_under_le_for_cumulative_cap() {
        let running = ValueExpr::Term(Term::Literal(Value::Decimal("60".to_string())));
        let proposed = ValueExpr::Term(Term::Literal(Value::Decimal("40".to_string())));
        let cap = ValueExpr::Term(Term::Literal(Value::Decimal("100".to_string())));

        // 60 + 40 <= 100 admits (binding pass-through).
        let under_cap = le_(
            Box::new(ValueExpr::Arith {
                op: ArithOp::Add,
                left: Box::new(running.clone()),
                right: Box::new(proposed),
            }),
            Box::new(cap.clone()),
        );
        let matches = find_matches(
            &under_cap,
            &ctx(&State::from_claims(vec![]), &Bindings::new()),
        )
        .unwrap();
        assert_eq!(matches.len(), 1, "60 + 40 <= 100 should admit");

        // 60 + 50 <= 100 fails (empty match set).
        let over_cap = le_(
            Box::new(ValueExpr::Arith {
                op: ArithOp::Add,
                left: Box::new(running),
                right: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
                    "50".to_string(),
                )))),
            }),
            Box::new(cap),
        );
        let matches = find_matches(
            &over_cap,
            &ctx(&State::from_claims(vec![]), &Bindings::new()),
        )
        .unwrap();
        assert!(matches.is_empty(), "60 + 50 <= 100 should reject");
    }

    /// `Prop::Or` returns the concatenation of each branch's binding
    /// sets, with no deduplication, mirroring `find_conjunction`'s
    /// multiplicity-preserving convention. Pins the four load-bearing
    /// cases: one branch matches, both branches match (multiplicity
    /// preserved), neither branch matches (empty), and a branch with a
    /// fresh binding contributes its extension.
    #[test]
    fn or_returns_union_of_branch_binding_sets() {
        // State holds two A claims and one B claim. Different keys per
        // predicate so a branch's extensions are distinguishable.
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "A".into(),
                args: vec![EvalValue::Subject("a1".into())],
            },
            ClaimInstance {
                predicate: "A".into(),
                args: vec![EvalValue::Subject("a2".into())],
            },
            ClaimInstance {
                predicate: "B".into(),
                args: vec![EvalValue::Subject("b1".into())],
            },
        ]);

        let a_x = Prop::Claim {
            predicate: "A".into(),
            args: vec![Term::Var("x".into())],
        };
        let b_x = Prop::Claim {
            predicate: "B".into(),
            args: vec![Term::Var("x".into())],
        };
        let c_x = Prop::Claim {
            predicate: "C".into(),
            args: vec![Term::Var("x".into())],
        };

        // Both branches match: two A extensions + one B extension = 3.
        let both = Prop::Or(vec![a_x.clone(), b_x.clone()]);
        let matches = find_matches(&both, &ctx(&state, &Bindings::new())).unwrap();
        assert_eq!(
            matches.len(),
            3,
            "Or must concatenate every branch's binding extensions"
        );
        let bound_x: Vec<_> = matches
            .iter()
            .map(
                |b| match b.get(&Var::from("x")).expect("x bound in every extension") {
                    EvalValue::Subject(s) => s.as_str().to_string(),
                    _ => panic!("x must be a subject"),
                },
            )
            .collect();
        assert!(bound_x.contains(&"a1".to_string()));
        assert!(bound_x.contains(&"a2".to_string()));
        assert!(bound_x.contains(&"b1".to_string()));

        // One branch matches, one doesn't: only the matching branch's
        // extensions are returned.
        let one_matches = Prop::Or(vec![a_x.clone(), c_x.clone()]);
        let matches = find_matches(&one_matches, &ctx(&state, &Bindings::new())).unwrap();
        assert_eq!(
            matches.len(),
            2,
            "Or with one empty branch returns the other branch's matches"
        );

        // Neither branch matches: empty.
        let none = Prop::Or(vec![c_x.clone(), c_x]);
        let matches = find_matches(&none, &ctx(&state, &Bindings::new())).unwrap();
        assert!(
            matches.is_empty(),
            "Or with every branch empty produces an empty result"
        );

        // No deduplication: two branches admitting the same extension
        // appear twice, matching find_conjunction's convention.
        let dup = Prop::Or(vec![a_x.clone(), a_x]);
        let matches = find_matches(&dup, &ctx(&state, &Bindings::new())).unwrap();
        assert_eq!(
            matches.len(),
            4,
            "Or preserves multiplicity; identical branches double-count"
        );
    }

    /// `Prop::Xor` holds exactly when one operand matches and the other
    /// does not - the truth table of the `(a or b) and not (a and b)` it
    /// lowers to. Pins all four cases with ground (binding-free) operands.
    #[test]
    fn xor_holds_for_exactly_one_operand() {
        let l = Prop::Claim {
            predicate: "L".into(),
            args: vec![],
        };
        let r = Prop::Claim {
            predicate: "R".into(),
            args: vec![],
        };
        let xor = Prop::Xor(Box::new(l), Box::new(r));

        let l_claim = ClaimInstance {
            predicate: "L".into(),
            args: vec![],
        };
        let r_claim = ClaimInstance {
            predicate: "R".into(),
            args: vec![],
        };

        let holds = |claims: Vec<ClaimInstance>| {
            !find_matches(&xor, &ctx(&State::from_claims(claims), &Bindings::new()))
                .unwrap()
                .is_empty()
        };

        assert!(holds(vec![l_claim.clone()]), "left only: xor holds");
        assert!(holds(vec![r_claim.clone()]), "right only: xor holds");
        assert!(
            !holds(vec![l_claim.clone(), r_claim]),
            "both: xor fails (not exclusive)"
        );
        assert!(!holds(vec![]), "neither: xor fails");
    }

    // ============================================================
    // Prop::Pre - pre-state opt-in
    // ============================================================

    /// `pre(inner)` flips state lookup: the inner expression sees
    /// pre-state, the outer sees post (candidate). Pins the basic
    /// flip semantic with a single decimal-counter scenario.
    #[test]
    fn pre_flips_predicate_lookup_to_pre_state() {
        // pre_state has Counter(1); post (candidate) has Counter(2).
        let pre = State::from_claims(vec![ClaimInstance {
            predicate: "Counter".into(),
            args: vec![EvalValue::Decimal(rust_decimal::Decimal::from(1))],
        }]);
        let post = State::from_claims(vec![ClaimInstance {
            predicate: "Counter".into(),
            args: vec![EvalValue::Decimal(rust_decimal::Decimal::from(2))],
        }]);

        // Counter(n) and pre(Counter(m)) implies n = m + 1
        let body = Prop::Implies {
            left: Box::new(Prop::And(vec![
                Prop::Claim {
                    predicate: "Counter".into(),
                    args: vec![Term::Var("n".into())],
                },
                Prop::Pre(Box::new(Prop::Claim {
                    predicate: "Counter".into(),
                    args: vec![Term::Var("m".into())],
                })),
            ])),
            right: Box::new(Prop::Eq(
                Box::new(ValueExpr::Term(Term::Var("n".into()))),
                Box::new(ValueExpr::Arith {
                    op: ArithOp::Add,
                    left: Box::new(ValueExpr::Term(Term::Var("m".into()))),
                    right: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
                        "1".to_string(),
                    )))),
                }),
            )),
        };

        let matches = find_matches(&body, &ctx_with_pre(&post, &pre, &Bindings::new())).unwrap();
        assert_eq!(
            matches.len(),
            1,
            "Counter(2) and pre(Counter(1)) implies 2 = 1 + 1 should hold"
        );

        // Now post has Counter(5): with pre still Counter(1), the
        // rule `n = m + 1` reduces to `5 = 2`, which must reject.
        let bad_post = State::from_claims(vec![ClaimInstance {
            predicate: "Counter".into(),
            args: vec![EvalValue::Decimal(rust_decimal::Decimal::from(5))],
        }]);
        let matches =
            find_matches(&body, &ctx_with_pre(&bad_post, &pre, &Bindings::new())).unwrap();
        assert!(
            matches.is_empty(),
            "Counter(5) and pre(Counter(1)) implies 5 = 1 + 1 should reject"
        );
    }

    /// `Prop::Pre` in a context with no pre_state in scope errors
    /// `PreStateUnavailable`. This is what enforces the doctrine:
    /// derived-claim bodies, transformation `require` bodies, and
    /// standalone evaluator callers cannot reach for `pre()`.
    #[test]
    fn pre_without_pre_state_errors_pre_state_unavailable() {
        let post = State::from_claims(vec![]);
        let body = Prop::Pre(Box::new(Prop::Claim {
            predicate: "Anything".into(),
            args: vec![],
        }));
        let err = find_matches(&body, &ctx(&post, &Bindings::new())).expect_err("must error");
        assert!(matches!(err, EvalError::PreStateUnavailable), "got {err:?}");
    }

    /// Nested `pre(pre(x))` is also unavailable - the inner subtree
    /// inherits a cleared pre slot, so a second `Pre` finds nothing
    /// to swap into.
    #[test]
    fn nested_pre_errors_pre_state_unavailable() {
        let pre = State::from_claims(vec![]);
        let post = State::from_claims(vec![]);
        let body = Prop::Pre(Box::new(Prop::Pre(Box::new(Prop::Claim {
            predicate: "Anything".into(),
            args: vec![],
        }))));
        let err = find_matches(&body, &ctx_with_pre(&post, &pre, &Bindings::new()))
            .expect_err("nested pre must error");
        assert!(matches!(err, EvalError::PreStateUnavailable), "got {err:?}");
    }

    /// `pre(forall x in S: body)` and `forall x in S: pre(body)`
    /// differ when the iteration domain itself shifts between pre
    /// and post. Pins that distinction: with `S` admitted only in
    /// post (say, an account that did not yet exist), the first
    /// form quantifies over nothing (vacuously true), the second
    /// quantifies over the post-state members.
    #[test]
    fn pre_outside_forall_vs_inside_distinguish_iteration_domain() {
        let pre = State::from_claims(vec![]);
        let post = State::from_claims(vec![
            ClaimInstance {
                predicate: "Account".into(),
                args: vec![EvalValue::Subject("a1".into())],
            },
            ClaimInstance {
                predicate: "Balance".into(),
                args: vec![EvalValue::Subject("a1".into())],
            },
        ]);

        // `pre(forall a in Account: Balance(a))`: in pre-state there
        // are no Account claims, so the source is empty and the
        // body is vacuously satisfied.
        let outside = Prop::Pre(Box::new(Prop::Forall {
            binding: "a".into(),
            source: Box::new(Prop::Claim {
                predicate: "Account".into(),
                args: vec![Term::Var("a".into())],
            }),
            body: Box::new(Prop::Claim {
                predicate: "Balance".into(),
                args: vec![Term::Var("a".into())],
            }),
        }));
        let matches = find_matches(&outside, &ctx_with_pre(&post, &pre, &Bindings::new())).unwrap();
        assert!(
            !matches.is_empty(),
            "pre(forall over empty pre-state Account) is vacuously true"
        );

        // `forall a in Account: pre(Balance(a))`: iterates the
        // single post-state Account, and asks whether Balance(a)
        // held in pre. Pre has no Balance, so the body fails for
        // the iterated a.
        let inside = Prop::Forall {
            binding: "a".into(),
            source: Box::new(Prop::Claim {
                predicate: "Account".into(),
                args: vec![Term::Var("a".into())],
            }),
            body: Box::new(Prop::Pre(Box::new(Prop::Claim {
                predicate: "Balance".into(),
                args: vec![Term::Var("a".into())],
            }))),
        };
        let matches = find_matches(&inside, &ctx_with_pre(&post, &pre, &Bindings::new())).unwrap();
        assert!(
            matches.is_empty(),
            "forall over post Account where body asks pre(Balance) must fail when pre has no Balance"
        );
    }

    // ============================================================
    // Stmt::BindOne - the deterministic unique-lookup binding statement.
    //
    // Binding quartet:
    //   require  = gate; does not export bindings
    //   bind_one = unique lookup; exports bindings
    //   let      = compute a value expression
    // ============================================================

    /// One-statement parameterless transformation body. BindOne tests
    /// drive the full `propose` path, not `find_matches` directly, so
    /// the statement contract is exercised against a real transformation.
    fn single_stmt_transformation(name: &str, body: Vec<Stmt>) -> Transformation {
        ir_builder::transformation(name, vec![], body)
    }

    fn run(t: &Transformation, state: &State) -> Result<Outcome, EvalError> {
        let transition = Transition {
            transformation_name: t.name.clone(),
            args: vec![],
            actor: Subject::from("test_actor"),
        };
        propose(t, &transition, state, &[], &[])
    }

    /// `bind_one` with a uniquely matching claim binds the variable
    /// for use by subsequent statements.
    #[test]
    fn bind_one_with_unique_match_extends_bindings_for_subsequent_stmts() {
        use ir_builder::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Policy".into(),
            args: vec![
                EvalValue::Subject("p1".into()),
                EvalValue::Decimal(Decimal::new(100, 0)),
            ],
        }]);
        let t = single_stmt_transformation(
            "extract_then_assert",
            vec![
                bind_one(claim("Policy", vec![var("policy_id"), var("limit")])),
                assert_("Echo", vec![var("policy_id"), var("limit")]),
            ],
        );
        let Outcome::Accepted {
            asserted_claims, ..
        } = run(&t, &state).unwrap()
        else {
            panic!("expected Accepted");
        };
        assert_eq!(asserted_claims.len(), 1);
        assert_eq!(asserted_claims[0].predicate.as_str(), "Echo");
        assert_eq!(
            asserted_claims[0].args,
            vec![
                EvalValue::Subject("p1".into()),
                EvalValue::Decimal(Decimal::new(100, 0)),
            ],
            "bind_one must have bound policy_id and limit for the assert"
        );
    }

    /// `bind_one` against a state with no matching claim rejects
    /// lawfully. The rejection reason names the expression so
    /// debugging is possible from the reason alone.
    #[test]
    fn bind_one_with_zero_matches_rejects_with_named_predicate() {
        use ir_builder::*;
        let state = State::default();
        let t = single_stmt_transformation(
            "extract_missing",
            vec![bind_one(claim(
                "Policy",
                vec![var("policy_id"), var("limit")],
            ))],
        );
        let Outcome::Rejected { reason } = run(&t, &state).unwrap() else {
            panic!("expected Rejected");
        };
        assert!(
            reason.to_string().contains("bind_one failed"),
            "reason should start with bind_one failed: {reason}"
        );
        assert!(
            reason.to_string().contains("Policy(policy_id, limit)"),
            "reason should name the expression: {reason}"
        );
    }

    /// The Display strings ARE the wire format: every envelope, trace
    /// entry, and rejection-log row renders the reason through Display,
    /// so these three strings are pinned byte-exactly. Changing one is
    /// a contract change, not a wording tweak.
    /// A witness reports a binding assignment, so it is empty exactly
    /// when there is none to report - not because of which operator
    /// failed. Here the whole body is a top-level `not`, which the
    /// drill-down does not enter and which binds nothing on the way.
    ///
    /// The complement is pinned by the worked examples: a comparison
    /// nested under an implication DOES witness, because the antecedent
    /// bound its variables before the comparison failed. Stating the rule
    /// in terms of operators, as an earlier draft of the docs did, gets
    /// that case backwards.
    #[test]
    fn a_failure_with_nothing_bound_has_an_empty_witness() {
        use ir_builder::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Flag".into(),
            args: vec![EvalValue::Subject("acct_1".into())],
        }]);
        let inv = invariant("no_flag_at_all", not(claim("Flag", vec![subj("acct_1")])));
        assert!(
            !eval_invariant(&inv, &state, None, &[]).unwrap(),
            "the flag is admitted, so the prohibition must fail"
        );
        assert!(
            invariant_witness(&inv, &state, None, &[])
                .unwrap()
                .is_empty(),
            "nothing was bound anywhere in this failure"
        );
    }

    #[test]
    fn rejection_reason_display_strings_are_pinned() {
        assert_eq!(
            RejectionReason::Invariant {
                name: "at_most_one".into(),
                version: 3,
                witness: Vec::new(),
            }
            .to_string(),
            "invariant `at_most_one` violated",
            "Display omits the version on purpose"
        );
        assert_eq!(
            RejectionReason::Invariant {
                name: "at_most_one".into(),
                version: 3,
                witness: vec![WitnessBinding {
                    var: "account".into(),
                    value: EvalValue::Subject("acct_42".into()),
                }],
            }
            .to_string(),
            "invariant `at_most_one` violated",
            "a witness must not leak into the pinned string; consumers read the field"
        );
        assert_eq!(
            RejectionReason::Require {
                name: None,
                rendered: "Approved(doc)".into(),
            }
            .to_string(),
            "require failed: Approved(doc) did not hold over pre-state"
        );
        // A named gate says which rule refused, mirroring the invariant
        // form. Unnamed stays byte-identical above, so every programme
        // written before names existed reports exactly as it did.
        assert_eq!(
            RejectionReason::Require {
                name: Some("approval_on_file".into()),
                rendered: "Approved(doc)".into(),
            }
            .to_string(),
            "require `approval_on_file` failed: Approved(doc) did not hold over pre-state"
        );
        assert_eq!(
            RejectionReason::BindNone {
                name: None,
                rendered: "Policy(policy_id, limit)".into(),
            }
            .to_string(),
            "bind_one failed: Policy(policy_id, limit) matched no candidates"
        );
        assert_eq!(
            RejectionReason::BindNone {
                name: Some("governing_policy".into()),
                rendered: "Policy(policy_id, limit)".into(),
            }
            .to_string(),
            "bind `governing_policy` failed: Policy(policy_id, limit) matched no candidates"
        );
    }

    /// Pins the kernel error `Display` strings byte-for-byte, one per
    /// tricky formatting class: a plain variant, a field-interpolated
    /// tuple, a `\`-continued long string, named struct fields with a
    /// nested `Display`, and a joined-expression message.
    #[test]
    fn eval_error_display_strings_are_pinned() {
        assert_eq!(EvalError::DivisionByZero.to_string(), "division by zero");
        assert_eq!(
            EvalError::UnboundVariable("amount".into()).to_string(),
            "unbound variable: amount"
        );
        assert_eq!(
            EvalError::UnknownDefinition("two_distinct".into()).to_string(),
            "call to definition `two_distinct` but the evaluation context carries \
             no such definition; validate the programme before proposing"
        );
    }

    #[test]
    fn validation_error_display_strings_are_pinned() {
        use crate::{ValidationContext, ValidationError, VocabularyKind};
        assert_eq!(
            ValidationError::Undeclared {
                vocabulary: VocabularyKind::Predicate,
                name: "Approved".into(),
                context: ValidationContext::Invariant {
                    name: "at_most_one".into(),
                },
            }
            .to_string(),
            "undeclared predicate `Approved` referenced in invariant `at_most_one`"
        );
        assert_eq!(
            ValidationError::DefinitionCycle {
                names: vec!["a".into(), "b".into()],
            }
            .to_string(),
            "definitions reference each other in a cycle (a, b); a definition \
             must expand to claims and conditions, never back to itself"
        );
        assert_eq!(
            ValidationError::DisciplinePointerCannotBeAppendOnly {
                predicate: "CurrentFigure".into(),
            }
            .to_string(),
            "`CurrentFigure` is declared both `append only` and `current \
             pointer`; a pointer must be retractable to move, which is the \
             opposite commitment - drop one"
        );
    }

    #[test]
    fn analysis_error_display_string_is_pinned() {
        assert_eq!(
            crate::analysis::AnalysisError::UnknownTransformation {
                name: "settle_trade".into(),
            }
            .to_string(),
            "unknown transformation `settle_trade`"
        );
    }

    /// `bind_one` against two matching claims surfaces a kernel error,
    /// not a lawful rejection: the programme expected unique state but
    /// admitted ambiguous state.
    #[test]
    fn bind_one_with_multiple_matches_is_kernel_error() {
        use ir_builder::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "Policy".into(),
                args: vec![
                    EvalValue::Subject("p1".into()),
                    EvalValue::Decimal(Decimal::new(100, 0)),
                ],
            },
            ClaimInstance {
                predicate: "Policy".into(),
                args: vec![
                    EvalValue::Subject("p2".into()),
                    EvalValue::Decimal(Decimal::new(200, 0)),
                ],
            },
        ]);
        let t = single_stmt_transformation(
            "ambiguous_lookup",
            vec![bind_one(claim(
                "Policy",
                vec![var("policy_id"), var("limit")],
            ))],
        );
        let err = run(&t, &state).expect_err("expected EvalError");
        match err {
            EvalError::TypeMismatch(msg) => {
                assert!(
                    msg.contains("bind_one matched 2 candidates"),
                    "error should report multiplicity: {msg}"
                );
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    /// A bind_one whose pattern uses an already-bound variable narrows
    /// the candidate set by that variable. With `policy_id` pre-bound
    /// (e.g. by an enclosing parameter or earlier bind_one), the
    /// pattern matches only the row carrying that policy_id.
    #[test]
    fn bind_one_with_pre_bound_var_constrains_match() {
        use ir_builder::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "Policy".into(),
                args: vec![
                    EvalValue::Subject("p1".into()),
                    EvalValue::Decimal(Decimal::new(100, 0)),
                ],
            },
            ClaimInstance {
                predicate: "Policy".into(),
                args: vec![
                    EvalValue::Subject("p2".into()),
                    EvalValue::Decimal(Decimal::new(200, 0)),
                ],
            },
        ]);
        // Two bind_ones in sequence: the first binds policy_id from
        // a literal subject; the second uses that binding to narrow
        // the Policy pattern. Without the narrowing, the second
        // bind_one would see two Policy candidates and error.
        let t = transformation(
            "narrow_by_var",
            vec![],
            vec![
                let_(
                    "policy_id",
                    term(Term::Literal(Value::Subject("p2".into()))),
                ),
                bind_one(claim("Policy", vec![var("policy_id"), var("limit")])),
                assert_("Echo", vec![var("limit")]),
            ],
        );
        let Outcome::Accepted {
            asserted_claims, ..
        } = run(&t, &state).unwrap()
        else {
            panic!("expected Accepted");
        };
        assert_eq!(
            asserted_claims[0].args,
            vec![EvalValue::Decimal(Decimal::new(200, 0))],
            "bound policy_id should narrow to p2's limit, not p1's"
        );
    }

    /// `bind_one` composes inside `For` bodies. Also pins the
    /// For-scoping rule: iteration 2 must not see iteration 1's `amt`
    /// binding, or its bind_one would narrow to the wrong row.
    #[test]
    fn bind_one_inside_for_body_composes() {
        use ir_builder::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "LineAmount".into(),
                args: vec![
                    EvalValue::Subject("L1".into()),
                    EvalValue::Decimal(Decimal::new(60, 0)),
                ],
            },
            ClaimInstance {
                predicate: "LineAmount".into(),
                args: vec![
                    EvalValue::Subject("L2".into()),
                    EvalValue::Decimal(Decimal::new(40, 0)),
                ],
            },
        ]);
        let t = transformation(
            "iterate_lines",
            vec!["lines".into()],
            vec![for_(
                "line",
                term(var("lines")),
                vec![
                    bind_one(claim("LineAmount", vec![var("line"), var("amt")])),
                    assert_("Echo", vec![var("line"), var("amt")]),
                ],
            )],
        );
        let transition = Transition {
            transformation_name: t.name.clone(),
            args: vec![EvalValue::Collection(vec![
                EvalValue::Subject("L1".into()),
                EvalValue::Subject("L2".into()),
            ])],
            actor: Subject::from("test_actor"),
        };
        let Outcome::Accepted {
            asserted_claims, ..
        } = propose(&t, &transition, &state, &[], &[]).unwrap()
        else {
            panic!("expected Accepted");
        };
        assert_eq!(asserted_claims.len(), 2);
        assert_eq!(asserted_claims[0].args[0], EvalValue::Subject("L1".into()));
        assert_eq!(
            asserted_claims[0].args[1],
            EvalValue::Decimal(Decimal::new(60, 0))
        );
        assert_eq!(asserted_claims[1].args[0], EvalValue::Subject("L2".into()));
        assert_eq!(
            asserted_claims[1].args[1],
            EvalValue::Decimal(Decimal::new(40, 0))
        );
    }

    /// `Term::Actor` resolves inside a `bind_one` expression, because
    /// `bind_one` runs inside a transformation body (which has a
    /// transition in scope). Authority-lookup patterns depend on this.
    #[test]
    fn bind_one_with_actor_in_pattern() {
        use ir_builder::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Authority".into(),
            args: vec![
                EvalValue::Subject("dr_smith".into()),
                EvalValue::Decimal(Decimal::new(50_000, 0)),
            ],
        }]);
        let t = transformation(
            "lookup_my_authority",
            vec![],
            vec![
                bind_one(claim("Authority", vec![actor(), var("limit")])),
                assert_("Echo", vec![var("limit")]),
            ],
        );
        let transition = Transition {
            transformation_name: t.name.clone(),
            args: vec![],
            actor: Subject::from("dr_smith"),
        };
        let Outcome::Accepted {
            asserted_claims, ..
        } = propose(&t, &transition, &state, &[], &[]).unwrap()
        else {
            panic!("expected Accepted");
        };
        assert_eq!(
            asserted_claims[0].args,
            vec![EvalValue::Decimal(Decimal::new(50_000, 0))]
        );
    }

    // The two-sort IR makes a value-producing expression inside
    // `bind_one` unrepresentable - `BindOne` holds a `Prop`, and `add`
    // builds a `ValueExpr` - so the former `bind_one_rejects_value_expr`
    // test (which depended on the now-deleted `EvalError::NotPredicate`)
    // no longer has a construction to exercise.

    // ============================================================
    // Program::validate() - strict arity validation.
    //
    // The validator collects every error rather than failing on the
    // first, so a migration sees the full work list in one re-run.
    // ============================================================

    /// Tiny one-claim programme with a `predicate` declaration that
    /// matches by default. Per-test mutations exercise each validator
    /// branch.
    fn one_claim_program() -> Program {
        use ir_builder::*;
        program("tiny")
            .predicates(vec![
                predicate("Echo").subject("id").decimal("amount").build(),
            ])
            .transformations(vec![transformation(
                "echo",
                params(&["id", "amount"]),
                vec![assert_("Echo", vec![var("id"), var("amount")])],
            )])
            .build()
    }

    #[test]
    fn validate_succeeds_when_every_predicate_use_matches_declared_arity() {
        let p = one_claim_program();
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn validate_reports_undeclared_predicate_in_transformation_body() {
        use ir_builder::*;
        let mut p = one_claim_program();
        p.transformations[0]
            .body
            .push(assert_("MissingPredicate", vec![var("id")]));
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::Undeclared { vocabulary: VocabularyKind::Predicate, name, .. }
                    if name == "MissingPredicate"
            )),
            "expected Undeclared(Predicate, MissingPredicate); got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_arity_mismatch_in_transformation_body() {
        use ir_builder::*;
        let mut p = one_claim_program();
        // Echo is declared with arity 2; calling with 1 arg trips
        // ArityMismatch.
        p.transformations[0].body = vec![assert_("Echo", vec![var("id")])];
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::ArityMismatch {
                    vocabulary: VocabularyKind::Predicate,
                    name,
                    expected: 2,
                    actual: 1,
                    ..
                } if name == "Echo"
            )),
            "expected ArityMismatch(Echo, 2, 1); got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_arity_mismatch_in_invariant_body() {
        use ir_builder::*;
        let mut p = one_claim_program();
        p.invariants.push(invariant(
            "bad_inv", // Echo has arity 2; invariant body uses arity 3.
            claim("Echo", vec![var("x"), var("y"), var("z")]),
        ));
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::ArityMismatch {
                    vocabulary: VocabularyKind::Predicate,
                    name: pred_name,
                    expected: 2,
                    actual: 3,
                    context: ValidationContext::Invariant { name },
                    ..
                } if pred_name == "Echo" && name == "bad_inv"
            )),
            "expected ArityMismatch in invariant context; got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_duplicate_predicate_decl() {
        use ir_builder::*;
        let mut p = one_claim_program();
        p.predicates
            .push(predicate("Echo").subject("a").subject("b").build());
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::DuplicateDecl { vocabulary: VocabularyKind::Predicate, name }
                    if name == "Echo"
            )),
            "expected DuplicateDecl(Predicate, Echo); got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_undeclared_derived_predicate() {
        use ir_builder::*;
        let mut p = one_claim_program();
        p.derived_claims.push(DerivedClaim {
            predicate: "Computed".into(),
            keys: vec!["id".into()],
            values: vec![DerivedValue {
                name: "n".into(),
                expr: term(var("id")),
            }],
            domain: claim("Echo", vec![var("id"), wildcard()]),
        });
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::Undeclared { vocabulary: VocabularyKind::Predicate, name, .. }
                    if name == "Computed"
            )),
            "expected Undeclared(Predicate, Computed); got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_derived_claim_arity_mismatch_against_declared_predicate() {
        use ir_builder::*;
        let mut p = one_claim_program();
        // Declare Computed with arity 3 but build it with keys=1,
        // values=1 (total arity 2 - one short).
        p.predicates.push(
            predicate("Computed")
                .subject("id")
                .subject("category")
                .decimal("balance")
                .build(),
        );
        p.derived_claims.push(DerivedClaim {
            predicate: "Computed".into(),
            keys: vec!["id".into()],
            values: vec![DerivedValue {
                name: "balance".into(),
                expr: term(var("id")),
            }],
            domain: claim("Echo", vec![var("id"), wildcard()]),
        });
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::ArityMismatch {
                    vocabulary: VocabularyKind::Predicate,
                    name,
                    expected: 3,
                    actual: 2,
                    context: ValidationContext::DerivedClaim { .. },
                    ..
                } if name == "Computed"
            )),
            "expected ArityMismatch on derived claim Computed; got: {errors:?}"
        );
    }

    /// The validator returns every error, not just the first.
    #[test]
    fn validate_returns_all_errors_not_just_the_first() {
        use ir_builder::*;
        let mut p = one_claim_program();
        p.transformations[0].body.push(assert_("MissingA", vec![]));
        p.transformations[0].body.push(assert_("MissingB", vec![]));
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.len() >= 2,
            "expected at least 2 errors; got: {errors:?}"
        );
        let names: Vec<&str> = errors
            .iter()
            .filter_map(|e| match e {
                ValidationError::Undeclared {
                    vocabulary: VocabularyKind::Predicate,
                    name,
                    ..
                } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"MissingA"));
        assert!(names.contains(&"MissingB"));
    }

    // ============================================================
    // propose_with_trace - structured per-statement diagnostic trace.
    //
    // The contract these pin: every statement that ran produces one
    // entry (For wraps its iterations in one); rejections produce
    // Completed { Rejected, trace }; kernel errors produce
    // Errored { error, trace } - the trace is NOT dropped on error.
    // ============================================================

    fn trace_transition(t: &Transformation, args: Vec<EvalValue>) -> Transition {
        Transition {
            transformation_name: t.name.clone(),
            args,
            actor: Subject::from("trace_actor"),
        }
    }

    /// Happy-path trace: every statement variant produces one entry,
    /// invariant checks appear at the end, the overall outcome is
    /// Accepted.
    #[test]
    fn propose_with_trace_records_every_statement_on_accept() {
        use ir_builder::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Policy".into(),
            args: vec![
                EvalValue::Subject("p1".into()),
                EvalValue::Decimal(Decimal::new(100, 0)),
            ],
        }]);
        let t = transformation(
            "happy",
            vec!["pid".into()],
            vec![
                require(claim("Policy", vec![var("pid"), wildcard()])),
                bind_one(claim("Policy", vec![var("pid"), var("limit")])),
                let_("doubled", add(term(var("limit")), term(var("limit")))),
                let_new_subject("new_id"),
                assert_("Echo", vec![var("new_id"), var("doubled")]),
                emit("EchoEmitted", vec![var("new_id")]),
            ],
        );
        let transition = trace_transition(&t, vec![EvalValue::Subject("p1".into())]);
        let TracedProposal::Completed { outcome, trace } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        assert!(matches!(outcome, Outcome::Accepted { .. }));
        assert_eq!(trace.len(), 6, "expected 6 entries, got: {trace:#?}");
        assert!(matches!(
            trace[0],
            TraceEntry::Require {
                outcome: RequireOutcome::Held { match_count: 1 },
                ..
            }
        ));
        assert!(matches!(trace[1], TraceEntry::BindOne { .. }));
        assert!(matches!(trace[2], TraceEntry::Let { .. }));
        assert!(matches!(trace[3], TraceEntry::LetNewSubject { .. }));
        assert!(matches!(trace[4], TraceEntry::Assert { .. }));
        assert!(matches!(trace[5], TraceEntry::Emit { .. }));
    }

    /// Require rejection: trace contains the failing entry, outcome
    /// is Rejected. The rendered expression appears verbatim in the
    /// trace, so callers can assert on the failing predicate name
    /// instead of pattern-matching on reason strings.
    #[test]
    fn propose_with_trace_records_failing_require_with_rendered_expression() {
        use ir_builder::*;
        let state = State::default();
        let t = transformation(
            "needs_policy",
            vec![],
            vec![require(claim(
                "Policy",
                vec![Term::Literal(Value::Subject("p1".into())), wildcard()],
            ))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { outcome, trace } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        assert!(matches!(outcome, Outcome::Rejected { .. }));
        assert_eq!(trace.len(), 1);
        let TraceEntry::Require {
            expression,
            outcome: RequireOutcome::Rejected { .. },
            ..
        } = &trace[0]
        else {
            panic!("expected require Rejected, got {:?}", trace[0]);
        };
        assert!(expression.contains("Policy"));
    }

    /// BindOne zero-match: trace shows NoMatch outcome with the
    /// expression.
    #[test]
    fn propose_with_trace_records_bind_one_no_match() {
        use ir_builder::*;
        let state = State::default();
        let t = transformation(
            "lookup_missing",
            vec![],
            vec![bind_one(claim("Policy", vec![var("pid"), var("limit")]))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { outcome, trace } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        assert!(matches!(outcome, Outcome::Rejected { .. }));
        assert_eq!(trace.len(), 1);
        assert!(matches!(
            trace[0],
            TraceEntry::BindOne {
                outcome: BindOneOutcome::NoMatch { .. },
                ..
            }
        ));
    }

    /// BindOne unique match: trace records the full bound binding set,
    /// sorted by variable name. The "replace, not extend" doctrine
    /// means the trace shows the new authoritative context, not a delta.
    #[test]
    fn propose_with_trace_records_bind_one_bound_with_sorted_bindings() {
        use ir_builder::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Policy".into(),
            args: vec![
                EvalValue::Subject("p1".into()),
                EvalValue::Decimal(Decimal::new(100, 0)),
            ],
        }]);
        let t = transformation(
            "lookup",
            vec![],
            vec![bind_one(claim("Policy", vec![var("pid"), var("limit")]))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        assert_eq!(trace.len(), 1);
        let TraceEntry::BindOne {
            outcome: BindOneOutcome::Bound { bindings },
            ..
        } = &trace[0]
        else {
            panic!("expected Bound, got {:?}", trace[0]);
        };
        // Sorted by variable name: limit, pid.
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].var.as_str(), "limit");
        assert_eq!(bindings[1].var.as_str(), "pid");
    }

    /// BindOne multi-match is a kernel error. The trace MUST still
    /// carry the entry showing why - dropping the trace on Err is
    /// exactly the case the trace-on-both-paths contract prevents.
    #[test]
    fn propose_with_trace_preserves_trace_on_bind_one_multi_match_error() {
        use ir_builder::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "Policy".into(),
                args: vec![
                    EvalValue::Subject("p1".into()),
                    EvalValue::Decimal(Decimal::new(100, 0)),
                ],
            },
            ClaimInstance {
                predicate: "Policy".into(),
                args: vec![
                    EvalValue::Subject("p2".into()),
                    EvalValue::Decimal(Decimal::new(200, 0)),
                ],
            },
        ]);
        let t = transformation(
            "ambiguous",
            vec![],
            vec![bind_one(claim("Policy", vec![var("pid"), var("limit")]))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Errored { error, trace } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Errored");
        };
        assert!(matches!(error, EvalError::TypeMismatch(_)));
        assert_eq!(trace.len(), 1);
        assert!(matches!(
            trace[0],
            TraceEntry::BindOne {
                outcome: BindOneOutcome::MultipleMatches { count: 2 },
                ..
            }
        ));
    }

    /// Retract trace carries the **actual retracted claims**, not
    /// just a count. Wildcard retractions that take out the wrong
    /// thing are exactly where debugging gets hard; the trace must
    /// show what was removed.
    #[test]
    fn propose_with_trace_records_retract_with_actual_claims() {
        use ir_builder::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "MayApprove".into(),
                args: vec![EvalValue::Subject("alice".into())],
            },
            ClaimInstance {
                predicate: "MayApprove".into(),
                args: vec![EvalValue::Subject("bob".into())],
            },
        ]);
        let t = transformation(
            "wildcard_retract",
            vec![],
            vec![retract("MayApprove", vec![wildcard()])],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        assert_eq!(trace.len(), 1);
        let TraceEntry::Retract { retracted, .. } = &trace[0] else {
            panic!("expected Retract, got {:?}", trace[0]);
        };
        assert_eq!(retracted.len(), 2);
    }

    /// For trace nests: outer trace gets one For entry, the inner
    /// per-iteration traces carry the iteration items so a caller
    /// can attribute a failing iteration to its element.
    #[test]
    fn propose_with_trace_records_for_with_per_iteration_items() {
        use ir_builder::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "LineAmount".into(),
                args: vec![
                    EvalValue::Subject("L1".into()),
                    EvalValue::Decimal(Decimal::new(60, 0)),
                ],
            },
            ClaimInstance {
                predicate: "LineAmount".into(),
                args: vec![
                    EvalValue::Subject("L2".into()),
                    EvalValue::Decimal(Decimal::new(40, 0)),
                ],
            },
        ]);
        let t = transformation(
            "iterate",
            vec!["lines".into()],
            vec![for_(
                "line",
                term(var("lines")),
                vec![bind_one(claim("LineAmount", vec![var("line"), var("amt")]))],
            )],
        );
        let transition = trace_transition(
            &t,
            vec![EvalValue::Collection(vec![
                EvalValue::Subject("L1".into()),
                EvalValue::Subject("L2".into()),
            ])],
        );
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        assert_eq!(trace.len(), 1);
        let TraceEntry::For { iterations, .. } = &trace[0] else {
            panic!("expected For, got {:?}", trace[0]);
        };
        assert_eq!(iterations.len(), 2);
        assert_eq!(iterations[0].item, EvalValue::Subject("L1".into()));
        assert_eq!(iterations[1].item, EvalValue::Subject("L2".into()));
        // Each iteration's inner trace has one bind_one entry.
        assert_eq!(iterations[0].trace.len(), 1);
        assert!(matches!(iterations[0].trace[0], TraceEntry::BindOne { .. }));
    }

    /// Invariant check: trace records one InvariantCheck per
    /// invariant, with the rendered body expression. An invariant
    /// rejection produces the entry plus an Outcome::Rejected.
    #[test]
    fn propose_with_trace_records_invariant_check_and_failure() {
        use ir_builder::*;
        let state = State::default();
        let t = transformation(
            "fires_invariant",
            vec![],
            vec![assert_(
                "X",
                vec![Term::Literal(Value::Subject("x1".into()))],
            )],
        );
        // Invariant: claim X(x1) must imply Y(x1). The transformation
        // asserts X but not Y, so the invariant fails on the
        // candidate state.
        let inv = invariant(
            "x_implies_y",
            implies(
                claim("X", vec![Term::Literal(Value::Subject("x1".into()))]),
                claim("Y", vec![Term::Literal(Value::Subject("x1".into()))]),
            ),
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { outcome, trace } =
            propose_with_trace(&t, &transition, &state, &[inv], &[])
        else {
            panic!("expected Completed");
        };
        assert!(matches!(outcome, Outcome::Rejected { .. }));
        // 1 assert entry + 1 invariant check entry.
        assert_eq!(trace.len(), 2);
        assert!(matches!(trace[0], TraceEntry::Assert { .. }));
        let TraceEntry::InvariantCheck {
            name,
            held,
            expression,
        } = &trace[1]
        else {
            panic!("expected InvariantCheck, got {:?}", trace[1]);
        };
        assert_eq!(name.as_str(), "x_implies_y");
        assert!(!held);
        assert!(expression.contains("implies"));
    }

    /// Sanity: `propose` (without trace) produces the same outcome
    /// as `propose_with_trace`. The two paths share an executor; if
    /// they ever diverged, this would catch it.
    #[test]
    fn propose_and_propose_with_trace_produce_identical_outcomes() {
        use ir_builder::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Policy".into(),
            args: vec![
                EvalValue::Subject("p1".into()),
                EvalValue::Decimal(Decimal::new(100, 0)),
            ],
        }]);
        let t = transformation(
            "lookup",
            vec![],
            vec![
                bind_one(claim("Policy", vec![var("pid"), var("limit")])),
                assert_("Echo", vec![var("pid"), var("limit")]),
            ],
        );
        let transition = trace_transition(&t, vec![]);
        let outcome_a = propose(&t, &transition, &state, &[], &[]).unwrap();
        let TracedProposal::Completed {
            outcome: outcome_b, ..
        } = propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        assert_eq!(outcome_a, outcome_b);
    }

    /// SPIKE parity: `propose_stage_delta` produces exactly the delta
    /// `propose` accepts with, and the same Require rejection when the
    /// body gates.
    #[test]
    fn propose_stage_delta_matches_propose_on_delta_and_body_rejection() {
        use ir_builder::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Policy".into(),
            args: vec![
                EvalValue::Subject("p1".into()),
                EvalValue::Decimal(Decimal::new(100, 0)),
            ],
        }]);
        let t = transformation(
            "lookup",
            vec![],
            vec![
                bind_one(claim("Policy", vec![var("pid"), var("limit")])),
                assert_("Echo", vec![var("pid"), var("limit")]),
            ],
        );
        let transition = trace_transition(&t, vec![]);
        let Outcome::Accepted {
            asserted_claims,
            retracted_claims,
            emitted_intents,
            ..
        } = propose(&t, &transition, &state, &[], &[]).unwrap()
        else {
            panic!("expected Accepted");
        };
        let StagedDelta::Staged {
            asserted,
            retracted,
            emitted,
        } = propose_stage_delta(&t, &transition, &state, &[]).unwrap()
        else {
            panic!("expected Staged");
        };
        assert_eq!(asserted, asserted_claims);
        assert_eq!(retracted, retracted_claims);
        assert_eq!(emitted, emitted_intents);

        let gated = transformation(
            "gated",
            vec![],
            vec![require(claim("Missing", vec![var("x")]))],
        );
        let transition = trace_transition(&gated, vec![]);
        let Outcome::Rejected { reason: kernel } =
            propose(&gated, &transition, &state, &[], &[]).unwrap()
        else {
            panic!("expected Rejected");
        };
        let StagedDelta::Rejected { reason: staged } =
            propose_stage_delta(&gated, &transition, &state, &[]).unwrap()
        else {
            panic!("expected Rejected");
        };
        assert_eq!(format!("{kernel}"), format!("{staged}"));
    }

    // ============================================================
    // Expression failure-walk.
    //
    // When `require` or `bind_one` rejects, the trace's
    // `failing_sub_expression` field carries the most specific
    // sub-expression responsible. These tests pin which expression
    // shapes drill in and which return None.
    // ============================================================

    fn extract_require_failure(trace: &[TraceEntry]) -> Option<&str> {
        trace.iter().find_map(|e| match e {
            TraceEntry::Require {
                outcome:
                    RequireOutcome::Rejected {
                        failing_sub_expression,
                        ..
                    },
                ..
            } => failing_sub_expression.as_deref(),
            _ => None,
        })
    }

    /// `And(A, B, C)` where the second conjunct fails: the walker
    /// renders the failing conjunct, not the whole And.
    #[test]
    fn failure_walk_and_points_at_first_failing_conjunct() {
        use ir_builder::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "A".into(),
            args: vec![EvalValue::Subject("x".into())],
        }]);
        // A holds (x is in state); B does not (no Bs in state).
        let t = transformation(
            "needs_a_and_b",
            vec![],
            vec![require(and(vec![
                claim("A", vec![Term::Literal(Value::Subject("x".into()))]),
                claim("B", vec![Term::Literal(Value::Subject("x".into()))]),
            ]))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        let failing = extract_require_failure(&trace).expect("expected failing sub-expression");
        assert!(
            failing.contains('B'),
            "expected failing sub-expression to contain B; got: {failing}"
        );
        assert!(
            !failing.contains("A("),
            "expected failing sub-expression NOT to be the whole And; got: {failing}"
        );
    }

    /// Nested And inside And: walker drills past the outer And to the
    /// inner failing conjunct.
    #[test]
    fn failure_walk_and_recurses_through_nested_and() {
        use ir_builder::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "A".into(),
            args: vec![EvalValue::Subject("x".into())],
        }]);
        // Outer And: [A, And(A, MissingPredicate)]. The nested And
        // fails at its second conjunct (MissingPredicate). Walker
        // should drill to that, not stop at the outer or inner And.
        let t = transformation(
            "nested_and",
            vec![],
            vec![require(and(vec![
                claim("A", vec![Term::Literal(Value::Subject("x".into()))]),
                and(vec![
                    claim("A", vec![Term::Literal(Value::Subject("x".into()))]),
                    claim(
                        "MissingPredicate",
                        vec![Term::Literal(Value::Subject("x".into()))],
                    ),
                ]),
            ]))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        let failing = extract_require_failure(&trace).expect("expected failing sub-expression");
        assert!(
            failing.contains("MissingPredicate"),
            "expected drill-down to leaf-most failing predicate; got: {failing}"
        );
        // Should NOT render as `and(...)` - that would mean we stopped
        // at the inner And without recursing.
        assert!(
            !failing.starts_with("and("),
            "expected drill past inner And; got: {failing}"
        );
    }

    /// `Implies(left, right)` where left holds and right fails:
    /// walker points at right.
    #[test]
    fn failure_walk_implies_points_at_right_when_left_holds() {
        use ir_builder::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Trigger".into(),
            args: vec![EvalValue::Subject("x".into())],
        }]);
        // Trigger(x) -> Required(x). Trigger holds, Required does not.
        let t = transformation(
            "needs_required_when_triggered",
            vec![],
            vec![require(implies(
                claim("Trigger", vec![Term::Literal(Value::Subject("x".into()))]),
                claim("Required", vec![Term::Literal(Value::Subject("x".into()))]),
            ))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        let failing = extract_require_failure(&trace).expect("expected failing sub-expression");
        assert!(
            failing.contains("Required"),
            "expected drill into right side of Implies; got: {failing}"
        );
    }

    /// `Forall { binding, source, body }` where the body fails for
    /// at least one source binding: walker drills into the body.
    #[test]
    fn failure_walk_forall_drills_into_body() {
        use ir_builder::*;
        // Source: a collection [x, y]. Body: claim "AllGood(line)".
        // State has AllGood(x) but not AllGood(y). The forall fails
        // at iteration y; walker should point at the body, not the
        // whole forall.
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "AllGood".into(),
            args: vec![EvalValue::Subject("x".into())],
        }]);
        let t = transformation(
            "all_lines_good",
            vec!["lines".into()],
            vec![require(forall(
                "line",
                in_(var("line"), var("lines")),
                claim("AllGood", vec![var("line")]),
            ))],
        );
        let transition = trace_transition(
            &t,
            vec![EvalValue::Collection(vec![
                EvalValue::Subject("x".into()),
                EvalValue::Subject("y".into()),
            ])],
        );
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        let failing = extract_require_failure(&trace).expect("expected failing sub-expression");
        assert!(
            failing.contains("AllGood"),
            "expected drill into forall body; got: {failing}"
        );
        assert!(
            !failing.starts_with("forall"),
            "expected drill past the forall wrapper; got: {failing}"
        );
    }

    /// `Not(inner)` failure: walker returns None. Not's failure means
    /// inner held; pointing at inner would say "this is what held"
    /// rather than "this is what failed", conflating two diagnostic
    /// models. Returning None is the safe choice in v0.
    #[test]
    fn failure_walk_not_returns_none() {
        use ir_builder::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Forbidden".into(),
            args: vec![EvalValue::Subject("x".into())],
        }]);
        // `not(Forbidden(x))` fails because Forbidden(x) holds.
        let t = transformation(
            "no_forbidden",
            vec![],
            vec![require(not(claim(
                "Forbidden",
                vec![Term::Literal(Value::Subject("x".into()))],
            )))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        let failing = trace.iter().find_map(|e| match e {
            TraceEntry::Require {
                outcome:
                    RequireOutcome::Rejected {
                        failing_sub_expression,
                        ..
                    },
                ..
            } => Some(failing_sub_expression.clone()),
            _ => None,
        });
        let failing = failing.expect("expected to find the Require entry");
        assert_eq!(
            failing, None,
            "Not failures should not produce a failing_sub_expression in v0"
        );
    }

    /// Leaf-shaped expression (a single Claim) that rejects: the
    /// walker returns None because the expression is already as
    /// specific as the kernel can be. The outer `expression` field
    /// of the trace entry already renders the leaf; duplicating it
    /// in `failing_sub_expression` adds no information.
    #[test]
    fn failure_walk_leaf_claim_returns_none() {
        use ir_builder::*;
        let state = State::default();
        let t = transformation(
            "needs_missing",
            vec![],
            vec![require(claim(
                "Missing",
                vec![Term::Literal(Value::Subject("x".into()))],
            ))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        let failing = trace.iter().find_map(|e| match e {
            TraceEntry::Require {
                outcome:
                    RequireOutcome::Rejected {
                        failing_sub_expression,
                        ..
                    },
                ..
            } => Some(failing_sub_expression.clone()),
            _ => None,
        });
        let failing = failing.expect("expected to find the Require entry");
        assert_eq!(
            failing, None,
            "Leaf failures (Claim, Le, etc.) should not produce a failing_sub_expression"
        );
    }

    /// BindOne zero-match: the walker also applies to bind_one's
    /// failure path. With a leaf-shaped Claim expression the result
    /// is None (same as require); the test pins that bind_one wires
    /// up the field at all.
    #[test]
    fn failure_walk_bind_one_no_match_carries_field() {
        use ir_builder::*;
        let state = State::default();
        let t = transformation(
            "lookup_missing",
            vec![],
            vec![bind_one(claim("Policy", vec![var("pid"), var("limit")]))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        let TraceEntry::BindOne {
            outcome:
                BindOneOutcome::NoMatch {
                    failing_sub_expression,
                    ..
                },
            ..
        } = &trace[0]
        else {
            panic!("expected BindOne NoMatch, got {:?}", trace[0]);
        };
        // Leaf-shaped: walker returns None. Field is present (the
        // value matters less than the structural presence).
        assert_eq!(failing_sub_expression.as_deref(), None);
    }

    // ============================================================
    // Additional failure-walk coverage
    // ============================================================

    /// Regression for the And binding-flow bug. The walker must
    /// thread bindings through conjuncts the same way the evaluator
    /// does. Without that, this case returns `None` because A(x) and
    /// B(x) each succeed against the original (empty) binding
    /// context - even though no x value satisfies both.
    #[test]
    fn failure_walk_and_threads_bindings_through_conjuncts() {
        use ir_builder::*;
        // A(a1) holds, B(b2) holds, but no x satisfies BOTH A(x) and
        // B(x).
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "A".into(),
                args: vec![EvalValue::Subject("a1".into())],
            },
            ClaimInstance {
                predicate: "B".into(),
                args: vec![EvalValue::Subject("b2".into())],
            },
        ]);
        let t = transformation(
            "needs_shared_x",
            vec![],
            vec![require(and(vec![
                claim("A", vec![var("x")]),
                claim("B", vec![var("x")]),
            ]))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        let failing = extract_require_failure(&trace);
        // Under the bug, this would be None (each conjunct evaluated
        // against the original empty bindings has matches). Under
        // the fix, after A binds x = a1, B(x = a1) fails - so B is
        // the failing conjunct.
        let failing = failing.expect(
            "binding-flow bug: walker should drill to the failing conjunct under threaded bindings",
        );
        assert!(
            failing.contains('B'),
            "expected failing conjunct B (no B(a1) in state); got: {failing}"
        );
    }

    /// `Implies(left, right)` where `left` itself fails: the implies
    /// is vacuously true at that branch, so a top-level rejection
    /// can't be attributed to either side meaningfully. Walker
    /// returns None.
    #[test]
    fn failure_walk_implies_with_failing_left_returns_none() {
        use ir_builder::*;
        // Trigger does not hold for x. Implies is vacuously true at
        // every iteration. But we need the implies to actually fail
        // overall to trigger the walker - so wrap it in an And with
        // a separately-failing conjunct, then assert that the walker
        // points at the failing And conjunct, not at the implies.
        let state = State::default();
        let t = transformation(
            "needs_failing_conjunct",
            vec![],
            vec![require(and(vec![
                // Implies with failing left: vacuously true; not a
                // useful drill-down target.
                implies(
                    claim("Trigger", vec![Term::Literal(Value::Subject("x".into()))]),
                    claim(
                        "Consequent",
                        vec![Term::Literal(Value::Subject("x".into()))],
                    ),
                ),
                // This conjunct genuinely fails.
                claim(
                    "RealRequirement",
                    vec![Term::Literal(Value::Subject("x".into()))],
                ),
            ]))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        let failing = extract_require_failure(&trace).expect("expected failing sub-expression");
        assert!(
            failing.contains("RealRequirement"),
            "expected the genuinely-failing conjunct, not the vacuous implies; got: {failing}"
        );
    }

    /// `Implies(left, right)` where right is itself compound: walker
    /// drills recursively into the failing inner sub-expression.
    #[test]
    fn failure_walk_implies_recurses_into_compound_right() {
        use ir_builder::*;
        // Trigger(x) holds; right is `And(StepA(x), StepB(x))`;
        // StepA holds, StepB fails. Walker should drill past Implies
        // and past the inner And to StepB.
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "Trigger".into(),
                args: vec![EvalValue::Subject("x".into())],
            },
            ClaimInstance {
                predicate: "StepA".into(),
                args: vec![EvalValue::Subject("x".into())],
            },
        ]);
        let t = transformation(
            "needs_both_steps",
            vec![],
            vec![require(implies(
                claim("Trigger", vec![Term::Literal(Value::Subject("x".into()))]),
                and(vec![
                    claim("StepA", vec![Term::Literal(Value::Subject("x".into()))]),
                    claim("StepB", vec![Term::Literal(Value::Subject("x".into()))]),
                ]),
            ))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        let failing = extract_require_failure(&trace).expect("expected failing sub-expression");
        assert!(
            failing.contains("StepB"),
            "expected drill-down through Implies + And to StepB; got: {failing}"
        );
    }

    /// `Forall` body recursion: when the body is itself compound,
    /// walker drills into the failing sub-expression of the body
    /// under the failing source binding.
    #[test]
    fn failure_walk_forall_recurses_into_compound_body() {
        use ir_builder::*;
        // Source: [x, y]. Body: And(A(line), B(line)). A holds for
        // both x and y; B only holds for x. Walker should drill into
        // the And and identify B as the failing conjunct under the y
        // iteration.
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "A".into(),
                args: vec![EvalValue::Subject("x".into())],
            },
            ClaimInstance {
                predicate: "A".into(),
                args: vec![EvalValue::Subject("y".into())],
            },
            ClaimInstance {
                predicate: "B".into(),
                args: vec![EvalValue::Subject("x".into())],
            },
        ]);
        let t = transformation(
            "every_line_has_a_and_b",
            vec!["lines".into()],
            vec![require(forall(
                "line",
                in_(var("line"), var("lines")),
                and(vec![
                    claim("A", vec![var("line")]),
                    claim("B", vec![var("line")]),
                ]),
            ))],
        );
        let transition = trace_transition(
            &t,
            vec![EvalValue::Collection(vec![
                EvalValue::Subject("x".into()),
                EvalValue::Subject("y".into()),
            ])],
        );
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        let failing = extract_require_failure(&trace).expect("expected failing sub-expression");
        assert!(
            failing.contains('B'),
            "expected drill past forall + into And, identifying B as failing; got: {failing}"
        );
        assert!(
            !failing.starts_with("forall") && !failing.starts_with("and("),
            "expected drill all the way to leaf; got: {failing}"
        );
    }

    /// `Exists` failure: structurally no single binding satisfied
    /// the body; pointing at the body would describe "what we
    /// looked for" rather than "what failed". Returns None.
    #[test]
    fn failure_walk_exists_returns_none() {
        use ir_builder::*;
        let state = State::default();
        let t = transformation(
            "needs_some_x",
            vec![],
            vec![require(exists("x", claim("Missing", vec![var("x")])))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        let failing = trace.iter().find_map(|e| match e {
            TraceEntry::Require {
                outcome:
                    RequireOutcome::Rejected {
                        failing_sub_expression,
                        ..
                    },
                ..
            } => Some(failing_sub_expression.clone()),
            _ => None,
        });
        assert_eq!(
            failing.expect("expected to find the Require entry"),
            None,
            "Exists failures should not produce a failing_sub_expression"
        );
    }

    /// `BindOne` with a compound expression: walker drills into the
    /// expression the same way it does for Require. Pin that the
    /// path is wired up symmetrically.
    #[test]
    fn failure_walk_bind_one_drills_into_compound_expression() {
        use ir_builder::*;
        // BindOne expects a unique match for And(Approved(x),
        // Active(x)). Approved holds for x; Active does not. The
        // walker should drill into the And and identify Active.
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Approved".into(),
            args: vec![EvalValue::Subject("x".into())],
        }]);
        let t = transformation(
            "unique_approved_and_active",
            vec![],
            vec![bind_one(and(vec![
                claim("Approved", vec![var("x")]),
                claim("Active", vec![var("x")]),
            ]))],
        );
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[], &[])
        else {
            panic!("expected Completed");
        };
        let TraceEntry::BindOne {
            outcome:
                BindOneOutcome::NoMatch {
                    failing_sub_expression,
                    ..
                },
            ..
        } = &trace[0]
        else {
            panic!("expected BindOne NoMatch, got {:?}", trace[0]);
        };
        let failing = failing_sub_expression
            .as_deref()
            .expect("expected drill-down on compound bind_one");
        assert!(
            failing.contains("Active"),
            "expected drill into BindOne's And to Active; got: {failing}"
        );
    }
}
