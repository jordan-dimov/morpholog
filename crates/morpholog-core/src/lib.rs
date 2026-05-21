//! Morpholog v0 semantic kernel.
//!
//! This crate is the synchronous, pure heart of Morpholog. It defines
//! the IR (invariants, transformations, claims, statements, expressions),
//! evaluates invariants against in-memory state, and exposes [`propose`],
//! the function that turns a proposed transformation into either an
//! accepted post-state or a rejected attempt.
//!
//! `morpholog-core` does no I/O. The PostgreSQL persistence adapter
//! lives in the separate `morpholog-postgres` crate and wraps this
//! kernel as an async boundary. Worked-example IR lives in the
//! `morpholog-examples` crate.
//!
//! Module layout:
//! - `ir` - IR types (`Invariant`, `Expr`, `Term`, `Value`, `Stmt`,
//!   `Claim`, `Intent`, `Transformation`, `Program`, `DerivedClaim`,
//!   `DerivedValue`, plus predicate-declaration types).
//! - `state` - Runtime state types: `EvalValue`, `ClaimInstance`,
//!   `IntentInstance`, `State`, `Bindings`.
//! - `eval` - The in-memory evaluator: `find_matches`, `eval_value`,
//!   `resolve_term`, `unify_args`, plus `EvalError`.
//! - `derive` - `eval_invariant` and `enumerate_derived`.
//! - `propose` - `propose`, `propose_with_trace`, and the trace types.
//! - `validate` - `Program::validate` machinery.
//! - `analysis` - Static analyses (`predicates_referenced_by_*`).
//! - `dsl` - Public IR-construction helpers.
//! - `format` - `format_program` and supporting renderers.
//!
//! The public API surface re-exports the items that callers (the PG
//! adapter, the CLI, the examples crate, downstream consumers) need.

pub mod dsl;
pub mod format;

mod analysis;
mod derive;
mod eval;
mod ir;
mod propose;
mod state;
mod validate;

pub use analysis::{
    predicates_read_by_stmt, predicates_referenced_by_derived, predicates_referenced_by_expr,
    predicates_referenced_by_stmt,
};
pub use derive::{enumerate_derived, eval_invariant};
pub use eval::EvalError;
pub use ir::{
    Claim, DerivedClaim, DerivedValue, Expr, Intent, Invariant, PredicateArgDecl, PredicateArgKind,
    PredicateDecl, Program, Stmt, Term, Transformation, Value,
};
pub use propose::{
    BindOneOutcome, ForIterationTrace, Outcome, RequireOutcome, TraceEntry, TracedProposal,
    Transition, propose, propose_with_trace,
};
pub use state::{ClaimInstance, EvalValue, IntentInstance, State};
pub use validate::{ValidationContext, ValidationError};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Kernel-internal unit tests for IR literals.
    //!
    //! Tests that depend on private items (`unify_args`, `resolve_term`,
    //! `Bindings`) live here. Tests that exercise the public surface —
    //! example chains, codec round-trips, IR-shape assertions — live in
    //! the `tests/` directory as integration tests, one file per example
    //! plus `tests/codec.rs` and the shared `tests/common/mod.rs`.

    use super::*;
    use crate::eval::{eval_value, find_matches, resolve_term, unify_args};
    use crate::state::Bindings;
    use jiff::civil::Date;
    use rust_decimal::Decimal;
    use std::collections::BTreeSet;

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
        let v = Value::Subject("bank_debt_service".to_string());
        assert_eq!(
            Term::Literal(v),
            Term::Literal(Value::Subject("bank_debt_service".to_string()))
        );
        let resolved = resolve_term(
            &Term::Literal(Value::Subject("bank_debt_service".to_string())),
            &Bindings::new(),
            None,
        )
        .unwrap();
        assert_eq!(
            resolved,
            EvalValue::Subject("bank_debt_service".to_string())
        );
    }

    #[test]
    fn subject_literal_unifies_with_matching_subject_arg() {
        let pattern = vec![Term::Literal(Value::Subject("p1".to_string()))];
        let value = vec![EvalValue::Subject("p1".to_string())];
        assert!(unify_args(&pattern, &value, &Bindings::new(), None).is_some());

        let mismatch = vec![EvalValue::Subject("p2".to_string())];
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
            predicate: "A".to_string(),
            args: vec![EvalValue::Subject("a1".to_string())],
        };
        let b1 = ClaimInstance {
            predicate: "B".to_string(),
            args: vec![EvalValue::Decimal(Decimal::new(42, 0))],
        };
        let a2 = ClaimInstance {
            predicate: "A".to_string(),
            args: vec![EvalValue::Subject("a2".to_string())],
        };
        let state = State::from_claims(vec![a1.clone(), b1.clone(), a2.clone()]);

        let a_rows: Vec<&ClaimInstance> = state.claims_for("A").collect();
        assert_eq!(a_rows.len(), 2, "two A claims admitted");
        assert!(a_rows.iter().all(|c| c.predicate == "A"));
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
            predicate: "JournalLine".to_string(),
            args: vec![
                EvalValue::Subject("entry_a".to_string()),
                EvalValue::Subject("account_cash".to_string()),
            ],
        };
        let line_for_entry_b = ClaimInstance {
            predicate: "JournalLine".to_string(),
            args: vec![
                EvalValue::Subject("entry_b".to_string()),
                EvalValue::Subject("account_cash".to_string()),
            ],
        };
        // Same value at position 0 but different predicate; must not
        // pollute the JournalLine[0=entry_a] bucket.
        let je_for_entry_a = ClaimInstance {
            predicate: "JournalEntry".to_string(),
            args: vec![EvalValue::Subject("entry_a".to_string())],
        };
        let state = State::from_claims(vec![
            line_for_entry_a.clone(),
            line_for_entry_b.clone(),
            je_for_entry_a.clone(),
        ]);

        let entry_a = EvalValue::Subject("entry_a".to_string());
        let positions = state
            .claim_indices_for_arg("JournalLine", 0, &entry_a)
            .expect("entry_a appears at JournalLine[0]");
        let claims: Vec<&ClaimInstance> = positions.iter().map(|&i| state.claim_at(i)).collect();
        assert_eq!(
            claims,
            vec![&line_for_entry_a],
            "must return only the JournalLine claim, not JournalEntry"
        );

        let unknown = EvalValue::Subject("entry_z".to_string());
        assert!(
            state
                .claim_indices_for_arg("JournalLine", 0, &unknown)
                .is_none(),
            "absent value returns None, signalling empty intersection"
        );

        let cash = EvalValue::Subject("account_cash".to_string());
        let cash_positions = state
            .claim_indices_for_arg("JournalLine", 1, &cash)
            .expect("account_cash appears at JournalLine[1]");
        assert_eq!(
            cash_positions.len(),
            2,
            "both JournalLine claims share account_cash at position 1"
        );
    }

    /// Pins the contract of `predicates_referenced_by_expr` by
    /// building an `Expr` that touches every variant carrying at
    /// least one nested `Expr` or `Claim`-shaped node. Each `Claim`
    /// and `ValueOf` site uses a unique predicate name. The
    /// extracted set must contain every planted name.
    ///
    /// This is the runtime safety net for the analysis. The
    /// compile-time safety net is the exhaustive `match` in
    /// `predicates_referenced_by_expr` itself: if a new `Expr`
    /// variant is added without handling, the function will not
    /// compile.
    #[test]
    fn predicates_referenced_by_expr_covers_every_variant() {
        // Helper to build a Claim-shaped Expr with a given predicate.
        let claim = |p: &str| Expr::Claim {
            predicate: p.to_string(),
            args: vec![],
        };
        // Helper to build a ValueOf-shaped Expr with a given predicate
        // and optionally a default expression that may carry more
        // predicates.
        let value_of = |p: &str, default: Option<Expr>| Expr::ValueOf {
            predicate: p.to_string(),
            args: vec![Term::Wildcard],
            default: default.map(Box::new),
        };

        let expr = Expr::And(vec![
            // Implies wraps two sides; both should be visited.
            Expr::Implies {
                left: Box::new(claim("P_implies_left")),
                right: Box::new(claim("P_implies_right")),
            },
            // Exists has a body.
            Expr::Exists {
                binding: "x".to_string(),
                body: Box::new(claim("P_exists_body")),
            },
            // Not wraps one expression.
            Expr::Not(Box::new(claim("P_not_body"))),
            // Eq operates on two sub-expressions.
            Expr::Eq(Box::new(claim("P_eq_left")), Box::new(claim("P_eq_right"))),
            // Le operates on two sub-expressions.
            Expr::Le(Box::new(claim("P_le_left")), Box::new(claim("P_le_right"))),
            // DateLe operates on two sub-expressions.
            Expr::DateLe(
                Box::new(claim("P_datele_left")),
                Box::new(claim("P_datele_right")),
            ),
            // Sub operates on two sub-expressions.
            Expr::Sub(
                Box::new(claim("P_sub_left")),
                Box::new(claim("P_sub_right")),
            ),
            // Add operates on two sub-expressions.
            Expr::Add(
                Box::new(claim("P_add_left")),
                Box::new(claim("P_add_right")),
            ),
            // Sum wraps a body.
            Expr::Sum {
                value: Term::Var("v".to_string()),
                binding: "v".to_string(),
                body: Box::new(claim("P_sum_body")),
            },
            // Forall has both source and body.
            Expr::Forall {
                binding: "y".to_string(),
                source: Box::new(claim("P_forall_source")),
                body: Box::new(claim("P_forall_body")),
            },
            // ValueOf carries its own predicate AND a recursive
            // default expression with another predicate.
            value_of("P_valueof_self", Some(claim("P_valueof_default"))),
            // Variants that carry no predicate references: must
            // contribute nothing. If any of these incorrectly added
            // entries the test below would still pass, but the
            // exhaustive set comparison further down catches
            // unexpected predicates too.
            Expr::Neq(Term::Var("a".to_string()), Term::Var("b".to_string())),
            Expr::Term(Term::Var("z".to_string())),
            Expr::In(Term::Var("e".to_string()), Term::Var("coll".to_string())),
        ]);

        let mut got = BTreeSet::new();
        predicates_referenced_by_expr(&expr, &mut got);

        let expected: BTreeSet<String> = [
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
            "P_sub_left",
            "P_sub_right",
            "P_add_left",
            "P_add_right",
            "P_sum_body",
            "P_forall_source",
            "P_forall_body",
            "P_valueof_self",
            "P_valueof_default",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        assert_eq!(
            got, expected,
            "every Expr variant that carries a predicate reference must contribute it"
        );
    }

    /// `predicates_read_by_stmt` includes every predicate the
    /// statement reads from pre-state (Require, BindOne, Let value,
    /// For collection + body, Retract pattern) and excludes
    /// `Stmt::Assert`'s output predicate.
    #[test]
    fn predicates_read_by_stmt_excludes_assert_includes_retract_and_reads() {
        use dsl::*;
        let body = vec![
            // Reads via require.
            require(claim("P_require", vec![var("x")])),
            // Reads via bind_one.
            bind_one(claim("P_bind", vec![var("y"), var("z")])),
            // Reads via Let value (claim is read).
            let_("v", claim("P_let", vec![var("y")])),
            // Writes only: P_assert MUST NOT appear in the read set.
            assert_("P_assert", vec![var("y")]),
            // Reads via retract pattern (pre-state matched).
            retract("P_retract", vec![wildcard()]),
            // For collection (read) + body (recurses).
            for_(
                "i",
                term(var("xs")),
                vec![require(claim("P_for_inner", vec![var("i")]))],
            ),
            // Intent: nothing.
            emit("Notified", vec![var("y")]),
        ];
        let mut got = BTreeSet::new();
        for stmt in &body {
            predicates_read_by_stmt(stmt, &mut got);
        }
        let expected: BTreeSet<String> =
            ["P_require", "P_bind", "P_let", "P_retract", "P_for_inner"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(
            got, expected,
            "read-set must include every pre-state read and exclude Stmt::Assert's output"
        );
        // Sanity: the broad walker DOES include P_assert.
        let mut broad = BTreeSet::new();
        for stmt in &body {
            predicates_referenced_by_stmt(stmt, &mut broad);
        }
        assert!(
            broad.contains("P_assert"),
            "broad walker must include Assert; got: {broad:?}"
        );
    }

    /// `Expr::Add` returns the decimal sum of its operands when both
    /// evaluate to decimals.
    #[test]
    fn add_sums_two_decimals() {
        let expr = Expr::Add(
            Box::new(Expr::Term(Term::Literal(Value::Decimal("10".to_string())))),
            Box::new(Expr::Term(Term::Literal(Value::Decimal(
                "32.5".to_string(),
            )))),
        );
        let v = eval_value(&expr, &State::from_claims(vec![]), &Bindings::new(), None).unwrap();
        assert_eq!(v, EvalValue::Decimal(Decimal::new(425, 1)));
    }

    /// Non-decimal operands surface as `TypeMismatch`. Same contract as
    /// `Sub`. Authority records and other claims that admit non-decimal
    /// values into an `Add` position must trip this rather than fall
    /// through silently.
    #[test]
    fn add_with_non_decimal_operand_is_type_mismatch() {
        let expr = Expr::Add(
            Box::new(Expr::Term(Term::Literal(Value::Decimal("10".to_string())))),
            Box::new(Expr::Term(Term::Literal(Value::Subject(
                "oops".to_string(),
            )))),
        );
        let err = eval_value(&expr, &State::from_claims(vec![]), &Bindings::new(), None)
            .expect_err("expected TypeMismatch");
        match err {
            EvalError::TypeMismatch(msg) => assert!(msg.contains("Add")),
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    fn date_lit(s: &str) -> Expr {
        Expr::Term(Term::Literal(Value::Date(s.to_string())))
    }

    /// `DateLe(a, b)` admits when `a < b`. The successful match
    /// returns the unchanged binding set, mirroring decimal `Le`.
    #[test]
    fn date_le_admits_before() {
        let expr = Expr::DateLe(
            Box::new(date_lit("2026-03-11")),
            Box::new(date_lit("2026-03-12")),
        );
        let matches =
            find_matches(&expr, &State::from_claims(vec![]), &Bindings::new(), None).unwrap();
        assert_eq!(matches.len(), 1, "earlier date must admit under DateLe");
    }

    /// Boundary case: equal dates admit. This pins the **inclusive**
    /// semantics of validity windows in v0 - `effective_to ==
    /// action_date` is admissible, not rejected. The clinical-trial
    /// enrolment example relies on this for "the protocol expires
    /// today" being a valid randomisation date.
    #[test]
    fn date_le_admits_equal() {
        let expr = Expr::DateLe(
            Box::new(date_lit("2026-03-12")),
            Box::new(date_lit("2026-03-12")),
        );
        let matches =
            find_matches(&expr, &State::from_claims(vec![]), &Bindings::new(), None).unwrap();
        assert_eq!(
            matches.len(),
            1,
            "equal dates must admit under DateLe (inclusive window semantics)"
        );
    }

    /// `DateLe(a, b)` with `a > b` returns no matches - the lawful
    /// rejection path, distinct from `TypeMismatch`.
    #[test]
    fn date_le_rejects_after() {
        let expr = Expr::DateLe(
            Box::new(date_lit("2026-03-13")),
            Box::new(date_lit("2026-03-12")),
        );
        let matches =
            find_matches(&expr, &State::from_claims(vec![]), &Bindings::new(), None).unwrap();
        assert!(matches.is_empty(), "later date must reject under DateLe");
    }

    /// Mixed operand kinds raise `TypeMismatch`, not silent rejection.
    /// The clinical-trial example must not be able to admit by mistake
    /// because someone passed a decimal where a date was expected.
    #[test]
    fn date_le_type_mismatch_decimal_vs_date() {
        let expr = Expr::DateLe(
            Box::new(Expr::Term(Term::Literal(Value::Decimal("1".to_string())))),
            Box::new(date_lit("2026-03-12")),
        );
        let err = find_matches(&expr, &State::from_claims(vec![]), &Bindings::new(), None)
            .expect_err("decimal lhs must be a TypeMismatch");
        match err {
            EvalError::TypeMismatch(msg) => assert!(msg.contains("DateLe")),
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    /// Symmetric to the above: a date on the left and a non-date on
    /// the right also raises `TypeMismatch`. Pins that the type guard
    /// covers both positions.
    #[test]
    fn date_le_type_mismatch_date_vs_subject() {
        let expr = Expr::DateLe(
            Box::new(date_lit("2026-03-12")),
            Box::new(Expr::Term(Term::Literal(Value::Subject(
                "oops".to_string(),
            )))),
        );
        let err = find_matches(&expr, &State::from_claims(vec![]), &Bindings::new(), None)
            .expect_err("subject rhs must be a TypeMismatch");
        match err {
            EvalError::TypeMismatch(msg) => assert!(msg.contains("DateLe")),
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    /// A malformed `Value::Date` source string surfaces as
    /// `TypeMismatch` at evaluation time, mirroring how an invalid
    /// `Value::Decimal` surfaces. There is no separate IR validation
    /// pass; parsing is the evaluator's concern.
    #[test]
    fn date_le_invalid_iso_string_is_type_mismatch() {
        let expr = Expr::DateLe(
            Box::new(date_lit("not-a-date")),
            Box::new(date_lit("2026-03-12")),
        );
        let err = find_matches(&expr, &State::from_claims(vec![]), &Bindings::new(), None)
            .expect_err("invalid ISO string must be a TypeMismatch");
        match err {
            EvalError::TypeMismatch(msg) => {
                assert!(msg.contains("invalid civil date"), "msg was: {msg}")
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    /// A `Value::Date` literal in a `claim` argument matches a
    /// claim admitted with the same date in that position. Pins the
    /// unify-against-literal-date path, the parallel of the existing
    /// decimal/subject literal unification.
    #[test]
    fn date_literal_unifies_with_matching_date_arg() {
        let claim = ClaimInstance {
            predicate: "OnDate".to_string(),
            args: vec![EvalValue::Date(
                "2026-03-12".parse::<Date>().expect("hand-built ISO date"),
            )],
        };
        let state = State::from_claims(vec![claim]);
        let expr = Expr::Claim {
            predicate: "OnDate".to_string(),
            args: vec![Term::Literal(Value::Date("2026-03-12".to_string()))],
        };
        let matches = find_matches(&expr, &state, &Bindings::new(), None).unwrap();
        assert_eq!(matches.len(), 1, "literal date arg must unify");

        let other = Expr::Claim {
            predicate: "OnDate".to_string(),
            args: vec![Term::Literal(Value::Date("2026-03-13".to_string()))],
        };
        let none = find_matches(&other, &state, &Bindings::new(), None).unwrap();
        assert!(
            none.is_empty(),
            "literal date arg must not unify with a different date"
        );
    }

    /// The cumulative-cap shape: `Le(Add(running, proposed), cap)`.
    /// This is the load-bearing composition the insurance-claim-settlement
    /// example uses to gate authorisations under a policy aggregate
    /// limit. Pinning it here so the kernel composition cannot drift.
    #[test]
    fn add_nests_under_le_for_cumulative_cap() {
        let running = Expr::Term(Term::Literal(Value::Decimal("60".to_string())));
        let proposed = Expr::Term(Term::Literal(Value::Decimal("40".to_string())));
        let cap = Expr::Term(Term::Literal(Value::Decimal("100".to_string())));

        // 60 + 40 <= 100 admits (binding pass-through).
        let under_cap = Expr::Le(
            Box::new(Expr::Add(Box::new(running.clone()), Box::new(proposed))),
            Box::new(cap.clone()),
        );
        let matches = find_matches(
            &under_cap,
            &State::from_claims(vec![]),
            &Bindings::new(),
            None,
        )
        .unwrap();
        assert_eq!(matches.len(), 1, "60 + 40 <= 100 should admit");

        // 60 + 50 <= 100 fails (empty match set).
        let over_cap = Expr::Le(
            Box::new(Expr::Add(
                Box::new(running),
                Box::new(Expr::Term(Term::Literal(Value::Decimal("50".to_string())))),
            )),
            Box::new(cap),
        );
        let matches = find_matches(
            &over_cap,
            &State::from_claims(vec![]),
            &Bindings::new(),
            None,
        )
        .unwrap();
        assert!(matches.is_empty(), "60 + 50 <= 100 should reject");
    }

    // ============================================================
    // Stmt::BindOne
    //
    // The deterministic unique-lookup binding statement. The
    // doctrine these tests pin:
    //
    //   require  = gate; does not export bindings
    //   bind_one = unique lookup; exports bindings
    //   let      = compute a value expression
    //
    // BindOne sits between Require (no binding export) and Let (a
    // value-producing expression). The tests below cover every
    // load-bearing branch: zero matches reject lawfully, one match
    // extends the binding context, two-or-more matches surface a
    // kernel error, the binding flows into subsequent statements,
    // and the existing NotPredicate path catches value-only
    // expressions slid into a BindOne by mistake.
    // ============================================================

    /// Build a one-statement transformation body containing the given
    /// statement, parameterless. Used by BindOne tests to drive the
    /// full `propose` path so we exercise the statement contract
    /// against a real transformation, not just `find_matches`.
    fn single_stmt_transformation(name: &str, body: Vec<Stmt>) -> Transformation {
        Transformation {
            name: name.to_string(),
            parameters: vec![],
            body,
        }
    }

    fn run(t: &Transformation, state: &State) -> Result<Outcome, EvalError> {
        let transition = Transition {
            transformation_name: t.name.clone(),
            args: vec![],
            actor: EvalValue::Subject("test_actor".to_string()),
        };
        propose(t, &transition, state, &[])
    }

    /// `bind_one` with a uniquely matching claim binds the variable
    /// for use by subsequent statements. Pinned against `propose`,
    /// not `execute_stmt` directly, so the test exercises the same
    /// path a real transformation does.
    #[test]
    fn bind_one_with_unique_match_extends_bindings_for_subsequent_stmts() {
        use dsl::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Policy".to_string(),
            args: vec![
                EvalValue::Subject("p1".to_string()),
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
        assert_eq!(asserted_claims[0].predicate, "Echo");
        assert_eq!(
            asserted_claims[0].args,
            vec![
                EvalValue::Subject("p1".to_string()),
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
        use dsl::*;
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
            reason.contains("bind_one failed"),
            "reason should start with bind_one failed: {reason}"
        );
        assert!(
            reason.contains("Policy(policy_id, limit)"),
            "reason should name the expression: {reason}"
        );
    }

    /// `bind_one` against a state with two matching claims surfaces
    /// a kernel error, not a lawful rejection. Two matches means
    /// the programme expected unique state but admitted ambiguous
    /// state - missing structural-uniqueness invariant or
    /// corruption.
    #[test]
    fn bind_one_with_multiple_matches_is_kernel_error() {
        use dsl::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "Policy".to_string(),
                args: vec![
                    EvalValue::Subject("p1".to_string()),
                    EvalValue::Decimal(Decimal::new(100, 0)),
                ],
            },
            ClaimInstance {
                predicate: "Policy".to_string(),
                args: vec![
                    EvalValue::Subject("p2".to_string()),
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
        use dsl::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "Policy".to_string(),
                args: vec![
                    EvalValue::Subject("p1".to_string()),
                    EvalValue::Decimal(Decimal::new(100, 0)),
                ],
            },
            ClaimInstance {
                predicate: "Policy".to_string(),
                args: vec![
                    EvalValue::Subject("p2".to_string()),
                    EvalValue::Decimal(Decimal::new(200, 0)),
                ],
            },
        ]);
        // Two bind_ones in sequence: the first binds policy_id from
        // a literal subject; the second uses that binding to narrow
        // the Policy pattern. Without the narrowing, the second
        // bind_one would see two Policy candidates and error.
        let t = Transformation {
            name: "narrow_by_var".to_string(),
            parameters: vec![],
            body: vec![
                let_(
                    "policy_id",
                    term(Term::Literal(Value::Subject("p2".to_string()))),
                ),
                bind_one(claim("Policy", vec![var("policy_id"), var("limit")])),
                assert_("Echo", vec![var("limit")]),
            ],
        };
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

    /// `bind_one` composes inside `For` bodies. The settlement-
    /// netting migration relies on this - the per-line value lookup
    /// (`bind_one LineAmount(line, amt)`) lives inside a
    /// `for line in lines:` body. Also pins the For-scoping fix:
    /// iteration 2 of the loop must not see iteration 1's `amt`
    /// binding, or its bind_one would narrow to the wrong row.
    #[test]
    fn bind_one_inside_for_body_composes() {
        use dsl::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "LineAmount".to_string(),
                args: vec![
                    EvalValue::Subject("L1".to_string()),
                    EvalValue::Decimal(Decimal::new(60, 0)),
                ],
            },
            ClaimInstance {
                predicate: "LineAmount".to_string(),
                args: vec![
                    EvalValue::Subject("L2".to_string()),
                    EvalValue::Decimal(Decimal::new(40, 0)),
                ],
            },
        ]);
        let t = Transformation {
            name: "iterate_lines".to_string(),
            parameters: vec!["lines".to_string()],
            body: vec![for_(
                "line",
                term(var("lines")),
                vec![
                    bind_one(claim("LineAmount", vec![var("line"), var("amt")])),
                    assert_("Echo", vec![var("line"), var("amt")]),
                ],
            )],
        };
        let transition = Transition {
            transformation_name: t.name.clone(),
            args: vec![EvalValue::Collection(vec![
                EvalValue::Subject("L1".to_string()),
                EvalValue::Subject("L2".to_string()),
            ])],
            actor: EvalValue::Subject("test_actor".to_string()),
        };
        let Outcome::Accepted {
            asserted_claims, ..
        } = propose(&t, &transition, &state, &[]).unwrap()
        else {
            panic!("expected Accepted");
        };
        assert_eq!(asserted_claims.len(), 2);
        assert_eq!(
            asserted_claims[0].args[0],
            EvalValue::Subject("L1".to_string())
        );
        assert_eq!(
            asserted_claims[0].args[1],
            EvalValue::Decimal(Decimal::new(60, 0))
        );
        assert_eq!(
            asserted_claims[1].args[0],
            EvalValue::Subject("L2".to_string())
        );
        assert_eq!(
            asserted_claims[1].args[1],
            EvalValue::Decimal(Decimal::new(40, 0))
        );
    }

    /// `Term::Actor` is resolvable inside a `bind_one` expression,
    /// because `bind_one` runs inside a transformation body (which
    /// has a transition in scope). Pinned because the
    /// `DelegatedInvestigator` pattern in the clinical-trial
    /// example - and any future authority lookup migrated to
    /// `bind_one` - depends on this.
    #[test]
    fn bind_one_with_actor_in_pattern() {
        use dsl::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Authority".to_string(),
            args: vec![
                EvalValue::Subject("dr_smith".to_string()),
                EvalValue::Decimal(Decimal::new(50_000, 0)),
            ],
        }]);
        let t = Transformation {
            name: "lookup_my_authority".to_string(),
            parameters: vec![],
            body: vec![
                bind_one(claim("Authority", vec![actor(), var("limit")])),
                assert_("Echo", vec![var("limit")]),
            ],
        };
        let transition = Transition {
            transformation_name: t.name.clone(),
            args: vec![],
            actor: EvalValue::Subject("dr_smith".to_string()),
        };
        let Outcome::Accepted {
            asserted_claims, ..
        } = propose(&t, &transition, &state, &[]).unwrap()
        else {
            panic!("expected Accepted");
        };
        assert_eq!(
            asserted_claims[0].args,
            vec![EvalValue::Decimal(Decimal::new(50_000, 0))]
        );
    }

    /// A value-producing expression (e.g. `Add`) inside `bind_one`
    /// surfaces as `EvalError::NotPredicate`, via the existing
    /// `find_matches` guardrail. Pinned because the public DSL
    /// permits the construction; the runtime is the right place
    /// to enforce the predicate-shaped contract.
    #[test]
    fn bind_one_rejects_value_expr_as_not_predicate() {
        use dsl::*;
        let state = State::default();
        let t = single_stmt_transformation(
            "misuse_value_expr",
            vec![bind_one(add(
                term(Term::Literal(Value::Decimal("1".to_string()))),
                term(Term::Literal(Value::Decimal("2".to_string()))),
            ))],
        );
        let err = run(&t, &state).expect_err("expected EvalError");
        assert!(
            matches!(err, EvalError::NotPredicate),
            "expected NotPredicate, got {err:?}"
        );
    }

    // ============================================================
    // Program::validate() - strict arity validation
    //
    // The tests below pin each ValidationError variant and the
    // happy path. The validator collects every error rather than
    // failing on the first; a programme migration that adds
    // predicate declarations should see the full work list at
    // once, not one item per re-run.
    // ============================================================

    /// Build a tiny one-claim programme with a `predicate` declaration
    /// that matches by default. Per-test mutations adjust the
    /// predicates list or the transformation body to exercise each
    /// validator branch.
    fn one_claim_program() -> Program {
        use dsl::*;
        Program {
            name: "tiny".to_string(),
            predicates: vec![predicate("Echo").subject("id").decimal("amount").build()],
            invariants: vec![],
            transformations: vec![Transformation {
                name: "echo".to_string(),
                parameters: params(&["id", "amount"]),
                body: vec![assert_("Echo", vec![var("id"), var("amount")])],
            }],
            derived_claims: vec![],
        }
    }

    #[test]
    fn validate_succeeds_when_every_predicate_use_matches_declared_arity() {
        let p = one_claim_program();
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn validate_reports_undeclared_predicate_in_transformation_body() {
        use dsl::*;
        let mut p = one_claim_program();
        p.transformations[0]
            .body
            .push(assert_("MissingPredicate", vec![var("id")]));
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::UndeclaredPredicate { predicate, .. }
                    if predicate == "MissingPredicate"
            )),
            "expected UndeclaredPredicate(MissingPredicate); got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_arity_mismatch_in_transformation_body() {
        use dsl::*;
        let mut p = one_claim_program();
        // Echo is declared with arity 2; calling with 1 arg trips
        // ArityMismatch.
        p.transformations[0].body = vec![assert_("Echo", vec![var("id")])];
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::ArityMismatch {
                    predicate,
                    expected: 2,
                    actual: 1,
                    ..
                } if predicate == "Echo"
            )),
            "expected ArityMismatch(Echo, 2, 1); got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_arity_mismatch_in_invariant_body() {
        use dsl::*;
        let mut p = one_claim_program();
        p.invariants.push(Invariant {
            name: "bad_inv".to_string(),
            version: 1,
            // Echo has arity 2; invariant body uses arity 3.
            body: claim("Echo", vec![var("x"), var("y"), var("z")]),
        });
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::ArityMismatch {
                    predicate,
                    expected: 2,
                    actual: 3,
                    context: ValidationContext::Invariant { name },
                    ..
                } if predicate == "Echo" && name == "bad_inv"
            )),
            "expected ArityMismatch in invariant context; got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_duplicate_predicate_decl() {
        use dsl::*;
        let mut p = one_claim_program();
        p.predicates
            .push(predicate("Echo").subject("a").subject("b").build());
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::DuplicatePredicateDecl { predicate }
                    if predicate == "Echo"
            )),
            "expected DuplicatePredicateDecl(Echo); got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_undeclared_derived_predicate() {
        use dsl::*;
        let mut p = one_claim_program();
        p.derived_claims.push(DerivedClaim {
            predicate: "Computed".to_string(),
            keys: vec!["id".to_string()],
            values: vec![DerivedValue {
                name: "n".to_string(),
                expr: term(var("id")),
            }],
            domain: claim("Echo", vec![var("id"), wildcard()]),
        });
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::UndeclaredPredicate { predicate, .. }
                    if predicate == "Computed"
            )),
            "expected UndeclaredPredicate(Computed); got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_derived_claim_arity_mismatch_against_declared_predicate() {
        use dsl::*;
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
            predicate: "Computed".to_string(),
            keys: vec!["id".to_string()],
            values: vec![DerivedValue {
                name: "balance".to_string(),
                expr: term(var("id")),
            }],
            domain: claim("Echo", vec![var("id"), wildcard()]),
        });
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::ArityMismatch {
                    predicate,
                    expected: 3,
                    actual: 2,
                    context: ValidationContext::DerivedClaim { .. },
                    ..
                } if predicate == "Computed"
            )),
            "expected ArityMismatch on derived claim Computed; got: {errors:?}"
        );
    }

    /// The validator collects every error and returns the full list.
    /// A migration that adds declarations should see all undeclared
    /// predicates at once, not one per re-run.
    #[test]
    fn validate_returns_all_errors_not_just_the_first() {
        use dsl::*;
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
                ValidationError::UndeclaredPredicate { predicate, .. } => Some(predicate.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"MissingA"));
        assert!(names.contains(&"MissingB"));
    }

    // ============================================================
    // propose_with_trace - structured per-statement diagnostic trace
    //
    // The tests below cover every TraceEntry variant on the happy
    // path, the rejection path with trace, and the kernel-error
    // path with partial trace. The contract these pin:
    //
    //   - Every statement that ran produces exactly one trace entry
    //     (For wraps its iterations in one entry).
    //   - Rejections (require/bind_one no-match, invariant failure)
    //     produce a Completed { Rejected, trace } including the
    //     failing entry.
    //   - Kernel errors (multi-match bind_one, evaluator errors)
    //     produce Errored { error, trace } - the trace is NOT
    //     dropped on the error path.
    //   - For traces nest with iteration items preserved.
    // ============================================================

    fn trace_transition(t: &Transformation, args: Vec<EvalValue>) -> Transition {
        Transition {
            transformation_name: t.name.clone(),
            args,
            actor: EvalValue::Subject("trace_actor".to_string()),
        }
    }

    /// Happy-path trace: every statement variant produces one entry,
    /// invariant checks appear at the end, the overall outcome is
    /// Accepted.
    #[test]
    fn propose_with_trace_records_every_statement_on_accept() {
        use dsl::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Policy".to_string(),
            args: vec![
                EvalValue::Subject("p1".to_string()),
                EvalValue::Decimal(Decimal::new(100, 0)),
            ],
        }]);
        let t = Transformation {
            name: "happy".to_string(),
            parameters: vec!["pid".to_string()],
            body: vec![
                require(claim("Policy", vec![var("pid"), wildcard()])),
                bind_one(claim("Policy", vec![var("pid"), var("limit")])),
                let_("doubled", add(term(var("limit")), term(var("limit")))),
                let_new_subject("new_id"),
                assert_("Echo", vec![var("new_id"), var("doubled")]),
                emit("EchoEmitted", vec![var("new_id")]),
            ],
        };
        let transition = trace_transition(&t, vec![EvalValue::Subject("p1".to_string())]);
        let TracedProposal::Completed { outcome, trace } =
            propose_with_trace(&t, &transition, &state, &[])
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
        use dsl::*;
        let state = State::default();
        let t = Transformation {
            name: "needs_policy".to_string(),
            parameters: vec![],
            body: vec![require(claim(
                "Policy",
                vec![Term::Literal(Value::Subject("p1".to_string())), wildcard()],
            ))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { outcome, trace } =
            propose_with_trace(&t, &transition, &state, &[])
        else {
            panic!("expected Completed");
        };
        assert!(matches!(outcome, Outcome::Rejected { .. }));
        assert_eq!(trace.len(), 1);
        let TraceEntry::Require {
            expression,
            outcome: RequireOutcome::Rejected { .. },
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
        use dsl::*;
        let state = State::default();
        let t = Transformation {
            name: "lookup_missing".to_string(),
            parameters: vec![],
            body: vec![bind_one(claim("Policy", vec![var("pid"), var("limit")]))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { outcome, trace } =
            propose_with_trace(&t, &transition, &state, &[])
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

    /// BindOne unique match: trace records the full bound binding
    /// set, sorted by variable name. PR B's "replace, not extend"
    /// doctrine means the trace shows the new authoritative context,
    /// not the delta.
    #[test]
    fn propose_with_trace_records_bind_one_bound_with_sorted_bindings() {
        use dsl::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Policy".to_string(),
            args: vec![
                EvalValue::Subject("p1".to_string()),
                EvalValue::Decimal(Decimal::new(100, 0)),
            ],
        }]);
        let t = Transformation {
            name: "lookup".to_string(),
            parameters: vec![],
            body: vec![bind_one(claim("Policy", vec![var("pid"), var("limit")]))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
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
        assert_eq!(bindings[0].0, "limit");
        assert_eq!(bindings[1].0, "pid");
    }

    /// BindOne multi-match is a kernel error. The trace MUST still
    /// carry the entry showing why - dropping the trace on Err is
    /// exactly the case this PR exists to prevent.
    #[test]
    fn propose_with_trace_preserves_trace_on_bind_one_multi_match_error() {
        use dsl::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "Policy".to_string(),
                args: vec![
                    EvalValue::Subject("p1".to_string()),
                    EvalValue::Decimal(Decimal::new(100, 0)),
                ],
            },
            ClaimInstance {
                predicate: "Policy".to_string(),
                args: vec![
                    EvalValue::Subject("p2".to_string()),
                    EvalValue::Decimal(Decimal::new(200, 0)),
                ],
            },
        ]);
        let t = Transformation {
            name: "ambiguous".to_string(),
            parameters: vec![],
            body: vec![bind_one(claim("Policy", vec![var("pid"), var("limit")]))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Errored { error, trace } =
            propose_with_trace(&t, &transition, &state, &[])
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
        use dsl::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "MayApprove".to_string(),
                args: vec![EvalValue::Subject("alice".to_string())],
            },
            ClaimInstance {
                predicate: "MayApprove".to_string(),
                args: vec![EvalValue::Subject("bob".to_string())],
            },
        ]);
        let t = Transformation {
            name: "wildcard_retract".to_string(),
            parameters: vec![],
            body: vec![retract("MayApprove", vec![wildcard()])],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
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
        use dsl::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "LineAmount".to_string(),
                args: vec![
                    EvalValue::Subject("L1".to_string()),
                    EvalValue::Decimal(Decimal::new(60, 0)),
                ],
            },
            ClaimInstance {
                predicate: "LineAmount".to_string(),
                args: vec![
                    EvalValue::Subject("L2".to_string()),
                    EvalValue::Decimal(Decimal::new(40, 0)),
                ],
            },
        ]);
        let t = Transformation {
            name: "iterate".to_string(),
            parameters: vec!["lines".to_string()],
            body: vec![for_(
                "line",
                term(var("lines")),
                vec![bind_one(claim("LineAmount", vec![var("line"), var("amt")]))],
            )],
        };
        let transition = trace_transition(
            &t,
            vec![EvalValue::Collection(vec![
                EvalValue::Subject("L1".to_string()),
                EvalValue::Subject("L2".to_string()),
            ])],
        );
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
        else {
            panic!("expected Completed");
        };
        assert_eq!(trace.len(), 1);
        let TraceEntry::For { iterations, .. } = &trace[0] else {
            panic!("expected For, got {:?}", trace[0]);
        };
        assert_eq!(iterations.len(), 2);
        assert_eq!(iterations[0].item, EvalValue::Subject("L1".to_string()));
        assert_eq!(iterations[1].item, EvalValue::Subject("L2".to_string()));
        // Each iteration's inner trace has one bind_one entry.
        assert_eq!(iterations[0].trace.len(), 1);
        assert!(matches!(iterations[0].trace[0], TraceEntry::BindOne { .. }));
    }

    /// Invariant check: trace records one InvariantCheck per
    /// invariant, with the rendered body expression. An invariant
    /// rejection produces the entry plus an Outcome::Rejected.
    #[test]
    fn propose_with_trace_records_invariant_check_and_failure() {
        use dsl::*;
        let state = State::default();
        let t = Transformation {
            name: "fires_invariant".to_string(),
            parameters: vec![],
            body: vec![assert_(
                "X",
                vec![Term::Literal(Value::Subject("x1".to_string()))],
            )],
        };
        // Invariant: claim X(x1) must imply Y(x1). The transformation
        // asserts X but not Y, so the invariant fails on the
        // candidate state.
        let inv = Invariant {
            name: "x_implies_y".to_string(),
            version: 1,
            body: implies(
                claim("X", vec![Term::Literal(Value::Subject("x1".to_string()))]),
                claim("Y", vec![Term::Literal(Value::Subject("x1".to_string()))]),
            ),
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { outcome, trace } =
            propose_with_trace(&t, &transition, &state, &[inv])
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
        assert_eq!(name, "x_implies_y");
        assert!(!held);
        assert!(expression.contains("implies"));
    }

    /// Sanity: `propose` (without trace) produces the same outcome
    /// as `propose_with_trace`. The two paths share an executor; if
    /// they ever diverged, this would catch it.
    #[test]
    fn propose_and_propose_with_trace_produce_identical_outcomes() {
        use dsl::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Policy".to_string(),
            args: vec![
                EvalValue::Subject("p1".to_string()),
                EvalValue::Decimal(Decimal::new(100, 0)),
            ],
        }]);
        let t = Transformation {
            name: "lookup".to_string(),
            parameters: vec![],
            body: vec![
                bind_one(claim("Policy", vec![var("pid"), var("limit")])),
                assert_("Echo", vec![var("pid"), var("limit")]),
            ],
        };
        let transition = trace_transition(&t, vec![]);
        let outcome_a = propose(&t, &transition, &state, &[]).unwrap();
        let TracedProposal::Completed {
            outcome: outcome_b, ..
        } = propose_with_trace(&t, &transition, &state, &[])
        else {
            panic!("expected Completed");
        };
        assert_eq!(outcome_a, outcome_b);
    }

    // ============================================================
    // Expression failure-walk (PR-G)
    //
    // When `require` or `bind_one` rejects, the trace's
    // `failing_sub_expression` field carries the most specific
    // sub-expression responsible. These tests pin the failure-walk
    // contract: which expression shapes drill in, which return None.
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
        use dsl::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "A".to_string(),
            args: vec![EvalValue::Subject("x".to_string())],
        }]);
        // A holds (x is in state); B does not (no Bs in state).
        let t = Transformation {
            name: "needs_a_and_b".to_string(),
            parameters: vec![],
            body: vec![require(and(vec![
                claim("A", vec![Term::Literal(Value::Subject("x".to_string()))]),
                claim("B", vec![Term::Literal(Value::Subject("x".to_string()))]),
            ]))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
        else {
            panic!("expected Completed");
        };
        let failing = extract_require_failure(&trace).expect("expected failing sub-expression");
        assert!(
            failing.contains("B"),
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
        use dsl::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "A".to_string(),
            args: vec![EvalValue::Subject("x".to_string())],
        }]);
        // Outer And: [A, And(A, MissingPredicate)]. The nested And
        // fails at its second conjunct (MissingPredicate). Walker
        // should drill to that, not stop at the outer or inner And.
        let t = Transformation {
            name: "nested_and".to_string(),
            parameters: vec![],
            body: vec![require(and(vec![
                claim("A", vec![Term::Literal(Value::Subject("x".to_string()))]),
                and(vec![
                    claim("A", vec![Term::Literal(Value::Subject("x".to_string()))]),
                    claim(
                        "MissingPredicate",
                        vec![Term::Literal(Value::Subject("x".to_string()))],
                    ),
                ]),
            ]))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
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
        use dsl::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Trigger".to_string(),
            args: vec![EvalValue::Subject("x".to_string())],
        }]);
        // Trigger(x) -> Required(x). Trigger holds, Required does not.
        let t = Transformation {
            name: "needs_required_when_triggered".to_string(),
            parameters: vec![],
            body: vec![require(implies(
                claim(
                    "Trigger",
                    vec![Term::Literal(Value::Subject("x".to_string()))],
                ),
                claim(
                    "Required",
                    vec![Term::Literal(Value::Subject("x".to_string()))],
                ),
            ))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
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
        use dsl::*;
        // Source: a collection [x, y]. Body: claim "AllGood(line)".
        // State has AllGood(x) but not AllGood(y). The forall fails
        // at iteration y; walker should point at the body, not the
        // whole forall.
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "AllGood".to_string(),
            args: vec![EvalValue::Subject("x".to_string())],
        }]);
        let t = Transformation {
            name: "all_lines_good".to_string(),
            parameters: vec!["lines".to_string()],
            body: vec![require(forall(
                "line",
                in_(var("line"), var("lines")),
                claim("AllGood", vec![var("line")]),
            ))],
        };
        let transition = trace_transition(
            &t,
            vec![EvalValue::Collection(vec![
                EvalValue::Subject("x".to_string()),
                EvalValue::Subject("y".to_string()),
            ])],
        );
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
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
        use dsl::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Forbidden".to_string(),
            args: vec![EvalValue::Subject("x".to_string())],
        }]);
        // `not(Forbidden(x))` fails because Forbidden(x) holds.
        let t = Transformation {
            name: "no_forbidden".to_string(),
            parameters: vec![],
            body: vec![require(not(claim(
                "Forbidden",
                vec![Term::Literal(Value::Subject("x".to_string()))],
            )))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
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
        use dsl::*;
        let state = State::default();
        let t = Transformation {
            name: "needs_missing".to_string(),
            parameters: vec![],
            body: vec![require(claim(
                "Missing",
                vec![Term::Literal(Value::Subject("x".to_string()))],
            ))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
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
        use dsl::*;
        let state = State::default();
        let t = Transformation {
            name: "lookup_missing".to_string(),
            parameters: vec![],
            body: vec![bind_one(claim("Policy", vec![var("pid"), var("limit")]))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
        else {
            panic!("expected Completed");
        };
        let TraceEntry::BindOne {
            outcome: BindOneOutcome::NoMatch {
                failing_sub_expression,
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
    // Additional failure-walk coverage (ChatGPT + Copilot PR #55)
    // ============================================================

    /// Regression for the And binding-flow bug. The walker must
    /// thread bindings through conjuncts the same way the evaluator
    /// does. Without that, this case returns `None` because A(x) and
    /// B(x) each succeed against the original (empty) binding
    /// context - even though no x value satisfies both.
    #[test]
    fn failure_walk_and_threads_bindings_through_conjuncts() {
        use dsl::*;
        // A(a1) holds, B(b2) holds, but no x satisfies BOTH A(x) and
        // B(x).
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "A".to_string(),
                args: vec![EvalValue::Subject("a1".to_string())],
            },
            ClaimInstance {
                predicate: "B".to_string(),
                args: vec![EvalValue::Subject("b2".to_string())],
            },
        ]);
        let t = Transformation {
            name: "needs_shared_x".to_string(),
            parameters: vec![],
            body: vec![require(and(vec![
                claim("A", vec![var("x")]),
                claim("B", vec![var("x")]),
            ]))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
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
            failing.contains("B"),
            "expected failing conjunct B (no B(a1) in state); got: {failing}"
        );
    }

    /// `Implies(left, right)` where `left` itself fails: the implies
    /// is vacuously true at that branch, so a top-level rejection
    /// can't be attributed to either side meaningfully. Walker
    /// returns None.
    #[test]
    fn failure_walk_implies_with_failing_left_returns_none() {
        use dsl::*;
        // Trigger does not hold for x. Implies is vacuously true at
        // every iteration. But we need the implies to actually fail
        // overall to trigger the walker - so wrap it in an And with
        // a separately-failing conjunct, then assert that the walker
        // points at the failing And conjunct, not at the implies.
        let state = State::default();
        let t = Transformation {
            name: "needs_failing_conjunct".to_string(),
            parameters: vec![],
            body: vec![require(and(vec![
                // Implies with failing left: vacuously true; not a
                // useful drill-down target.
                implies(
                    claim(
                        "Trigger",
                        vec![Term::Literal(Value::Subject("x".to_string()))],
                    ),
                    claim(
                        "Consequent",
                        vec![Term::Literal(Value::Subject("x".to_string()))],
                    ),
                ),
                // This conjunct genuinely fails.
                claim(
                    "RealRequirement",
                    vec![Term::Literal(Value::Subject("x".to_string()))],
                ),
            ]))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
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
        use dsl::*;
        // Trigger(x) holds; right is `And(StepA(x), StepB(x))`;
        // StepA holds, StepB fails. Walker should drill past Implies
        // and past the inner And to StepB.
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "Trigger".to_string(),
                args: vec![EvalValue::Subject("x".to_string())],
            },
            ClaimInstance {
                predicate: "StepA".to_string(),
                args: vec![EvalValue::Subject("x".to_string())],
            },
        ]);
        let t = Transformation {
            name: "needs_both_steps".to_string(),
            parameters: vec![],
            body: vec![require(implies(
                claim(
                    "Trigger",
                    vec![Term::Literal(Value::Subject("x".to_string()))],
                ),
                and(vec![
                    claim(
                        "StepA",
                        vec![Term::Literal(Value::Subject("x".to_string()))],
                    ),
                    claim(
                        "StepB",
                        vec![Term::Literal(Value::Subject("x".to_string()))],
                    ),
                ]),
            ))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
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
        use dsl::*;
        // Source: [x, y]. Body: And(A(line), B(line)). A holds for
        // both x and y; B only holds for x. Walker should drill into
        // the And and identify B as the failing conjunct under the y
        // iteration.
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "A".to_string(),
                args: vec![EvalValue::Subject("x".to_string())],
            },
            ClaimInstance {
                predicate: "A".to_string(),
                args: vec![EvalValue::Subject("y".to_string())],
            },
            ClaimInstance {
                predicate: "B".to_string(),
                args: vec![EvalValue::Subject("x".to_string())],
            },
        ]);
        let t = Transformation {
            name: "every_line_has_a_and_b".to_string(),
            parameters: vec!["lines".to_string()],
            body: vec![require(forall(
                "line",
                in_(var("line"), var("lines")),
                and(vec![
                    claim("A", vec![var("line")]),
                    claim("B", vec![var("line")]),
                ]),
            ))],
        };
        let transition = trace_transition(
            &t,
            vec![EvalValue::Collection(vec![
                EvalValue::Subject("x".to_string()),
                EvalValue::Subject("y".to_string()),
            ])],
        );
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
        else {
            panic!("expected Completed");
        };
        let failing = extract_require_failure(&trace).expect("expected failing sub-expression");
        assert!(
            failing.contains("B"),
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
        use dsl::*;
        let state = State::default();
        let t = Transformation {
            name: "needs_some_x".to_string(),
            parameters: vec![],
            body: vec![require(exists("x", claim("Missing", vec![var("x")])))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
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
        use dsl::*;
        // BindOne expects a unique match for And(Approved(x),
        // Active(x)). Approved holds for x; Active does not. The
        // walker should drill into the And and identify Active.
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Approved".to_string(),
            args: vec![EvalValue::Subject("x".to_string())],
        }]);
        let t = Transformation {
            name: "unique_approved_and_active".to_string(),
            parameters: vec![],
            body: vec![bind_one(and(vec![
                claim("Approved", vec![var("x")]),
                claim("Active", vec![var("x")]),
            ]))],
        };
        let transition = trace_transition(&t, vec![]);
        let TracedProposal::Completed { trace, .. } =
            propose_with_trace(&t, &transition, &state, &[])
        else {
            panic!("expected Completed");
        };
        let TraceEntry::BindOne {
            outcome: BindOneOutcome::NoMatch {
                failing_sub_expression,
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
