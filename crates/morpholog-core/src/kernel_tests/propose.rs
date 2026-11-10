//! Proposals: `bind`, the pinned error and reason renderings, and the
//! trace a proposal leaves on every path.

use super::*;

// Stmt::BindOne: unique lookup that exports its bindings, unlike
// `require`, which is only a gate.

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

/// `bind_one` with no matching claim rejects, and the reason names the
/// expression.
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

/// A witness is empty when nothing was bound, whatever operator failed.
/// Here the body is a top-level `not`, which binds nothing. (A comparison
/// under an implication does witness; the worked examples pin that.)
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
            .witness
            .is_empty(),
        "nothing was bound anywhere in this failure"
    );
}

/// These Display strings are the wire format for envelopes, traces and
/// the rejection log, so they are pinned byte for byte. Changing one is a
/// contract change.
#[test]
fn rejection_reason_display_strings_are_pinned() {
    assert_eq!(
        RejectionReason::Invariant {
            name: "at_most_one".into(),
            version: 3,
            witness: Vec::new(),
            compared: None,
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
            compared: None,
        }
        .to_string(),
        "invariant `at_most_one` violated",
        "a witness must not leak into the pinned string; consumers read the field"
    );
    assert_eq!(
        RejectionReason::Require {
            name: None,
            rendered: "Approved(doc)".into(),
            witness: vec![],
            compared: None,
        }
        .to_string(),
        "require failed: Approved(doc) did not hold over pre-state"
    );
    // A named gate says which rule refused; an unnamed one reads as
    // above.
    assert_eq!(
        RejectionReason::Require {
            name: Some("approval_on_file".into()),
            rendered: "Approved(doc)".into(),
            witness: vec![],
            compared: None,
        }
        .to_string(),
        "require `approval_on_file` failed: Approved(doc) did not hold over pre-state"
    );
    assert_eq!(
        RejectionReason::BindNone {
            name: None,
            rendered: "Policy(policy_id, limit)".into(),
            witness: vec![],
            compared: None,
        }
        .to_string(),
        "bind_one failed: Policy(policy_id, limit) matched no candidates"
    );
    assert_eq!(
        RejectionReason::BindNone {
            name: Some("governing_policy".into()),
            rendered: "Policy(policy_id, limit)".into(),
            witness: vec![],
            compared: None,
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

/// `bind_one` against two matching claims is a kernel error, not a
/// rejection: the programme expected one match and its state holds two.
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

/// A bind_one pattern using an already-bound variable matches only rows
/// with that value.
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
    // The first bind_one binds policy_id; the second uses it to narrow
    // Policy. Without that, it would see two candidates and error.
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

/// `bind_one` works inside a `For` body, and iteration 2 does not see
/// iteration 1's `amt` binding.
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
        decided_at: None,
    };
    let Outcome::Accepted {
        asserted_claims, ..
    } = propose(
        &t,
        &transition,
        &state,
        &[],
        &[],
        &mut crate::kernel_tests::fresh(),
    )
    .unwrap()
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

/// `Term::Actor` resolves inside a `bind_one` expression, as authority
/// lookups need.
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
        decided_at: None,
    };
    let Outcome::Accepted {
        asserted_claims, ..
    } = propose(
        &t,
        &transition,
        &state,
        &[],
        &[],
        &mut crate::kernel_tests::fresh(),
    )
    .unwrap()
    else {
        panic!("expected Accepted");
    };
    assert_eq!(
        asserted_claims[0].args,
        vec![EvalValue::Decimal(Decimal::new(50_000, 0))]
    );
}

// propose_with_trace: one entry per statement that ran (a For wraps its
// iterations in one). A rejection gives Completed { Rejected, trace }; a
// kernel error gives Errored { error, trace }, keeping the trace.

/// Accepted: one entry per statement, then the invariant checks.
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
    let TracedProposal::Completed { outcome, trace } = propose_with_trace(
        crate::propose::Vocabulary::undeclared(),
        &t,
        &transition,
        &state,
        &[],
        &[],
        &mut crate::kernel_tests::fresh(),
    ) else {
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

/// A require rejection: the trace holds the failing entry with its
/// rendered expression, so callers need not parse reason strings.
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
    let TracedProposal::Completed { outcome, trace } = propose_with_trace(
        crate::propose::Vocabulary::undeclared(),
        &t,
        &transition,
        &state,
        &[],
        &[],
        &mut crate::kernel_tests::fresh(),
    ) else {
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
    let TracedProposal::Completed { outcome, trace } = propose_with_trace(
        crate::propose::Vocabulary::undeclared(),
        &t,
        &transition,
        &state,
        &[],
        &[],
        &mut crate::kernel_tests::fresh(),
    ) else {
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

/// BindOne unique match: the trace records the whole new binding set,
/// sorted by variable, not a delta.
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
    let TracedProposal::Completed { trace, .. } = propose_with_trace(
        crate::propose::Vocabulary::undeclared(),
        &t,
        &transition,
        &state,
        &[],
        &[],
        &mut crate::kernel_tests::fresh(),
    ) else {
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

/// BindOne multi-match is a kernel error, and the trace still carries
/// the entry showing why.
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
    let TracedProposal::Errored { error, trace } = propose_with_trace(
        crate::propose::Vocabulary::undeclared(),
        &t,
        &transition,
        &state,
        &[],
        &[],
        &mut crate::kernel_tests::fresh(),
    ) else {
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

/// The retract trace lists the claims actually removed, not a count,
/// so a wildcard that removes the wrong thing is visible.
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
    let TracedProposal::Completed { trace, .. } = propose_with_trace(
        crate::propose::Vocabulary::undeclared(),
        &t,
        &transition,
        &state,
        &[],
        &[],
        &mut crate::kernel_tests::fresh(),
    ) else {
        panic!("expected Completed");
    };
    assert_eq!(trace.len(), 1);
    let TraceEntry::Retract { retracted, .. } = &trace[0] else {
        panic!("expected Retract, got {:?}", trace[0]);
    };
    assert_eq!(retracted.len(), 2);
}

/// A For trace nests: one For entry whose per-iteration traces carry
/// each item.
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
    let TracedProposal::Completed { trace, .. } = propose_with_trace(
        crate::propose::Vocabulary::undeclared(),
        &t,
        &transition,
        &state,
        &[],
        &[],
        &mut crate::kernel_tests::fresh(),
    ) else {
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

/// One InvariantCheck per checked invariant, with its rendered body. A
/// failure also yields Outcome::Rejected.
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
    // X(x1) implies Y(x1); the body admits X but not Y.
    let inv = invariant(
        "x_implies_y",
        implies(
            claim("X", vec![Term::Literal(Value::Subject("x1".into()))]),
            claim("Y", vec![Term::Literal(Value::Subject("x1".into()))]),
        ),
    );
    let transition = trace_transition(&t, vec![]);
    let TracedProposal::Completed { outcome, trace } = propose_with_trace(
        crate::propose::Vocabulary::undeclared(),
        &t,
        &transition,
        &state,
        &[inv],
        &[],
        &mut crate::kernel_tests::fresh(),
    ) else {
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

/// `propose` and `propose_with_trace` give the same outcome.
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
    let outcome_a = propose(
        &t,
        &transition,
        &state,
        &[],
        &[],
        &mut crate::kernel_tests::fresh(),
    )
    .unwrap();
    let TracedProposal::Completed {
        outcome: outcome_b, ..
    } = propose_with_trace(
        crate::propose::Vocabulary::undeclared(),
        &t,
        &transition,
        &state,
        &[],
        &[],
        &mut crate::kernel_tests::fresh(),
    )
    else {
        panic!("expected Completed");
    };
    assert_eq!(outcome_a, outcome_b);
}

/// The public API looks a transformation up by the transition's name, so
/// they cannot disagree there; the kernel still refuses a pair that does,
/// so an audit row can never name an act that did not run.
#[test]
fn a_transition_naming_another_transformation_is_an_error() {
    use ir_builder::*;
    let t = transformation("act", params(&[]), vec![]);
    let transition = Transition {
        transformation_name: "some_other_name".into(),
        args: vec![],
        actor: Subject::from("tester"),
        decided_at: None,
    };
    let err = propose(&t, &transition, &State::default(), &[], &[], &mut fresh())
        .expect_err("a mismatched name is an error, never an outcome");
    assert!(
        matches!(&err, EvalError::TypeMismatch(msg) if msg.contains("some_other_name") && msg.contains("act")),
        "{err:?}"
    );
}

// Stmt::If: a yes/no test that chooses a branch instead of refusing.

/// The branch the condition selects runs; the other does not, and only
/// the branch taken draws a subject.
#[test]
fn if_runs_the_branch_its_condition_selects_and_draws_only_there() {
    use ir_builder::*;
    let t = transformation(
        "choose",
        vec!["x".into()],
        vec![if_(
            claim("Flag", vec![var("x")]),
            vec![
                let_new_subject("fresh"),
                assert_("Then", vec![var("x"), var("fresh")]),
            ],
            vec![assert_("Else", vec![var("x")])],
        )],
    );
    let flagged = State::from_claims(vec![ClaimInstance {
        predicate: "Flag".into(),
        args: vec![EvalValue::Subject("a".into())],
    }]);
    let subjects_for = |state: &State| {
        let transition = trace_transition(&t, vec![EvalValue::Subject("a".into())]);
        let mut source = vec![Subject::from("s1"), Subject::from("s2")].into_iter();
        let outcome = propose(&t, &transition, state, &[], &[], &mut source).unwrap();
        (outcome, source.count())
    };
    let (taken, left) = subjects_for(&flagged);
    let Outcome::Accepted {
        asserted_claims, ..
    } = taken
    else {
        panic!("expected Accepted");
    };
    assert_eq!(asserted_claims.len(), 1);
    assert_eq!(asserted_claims[0].predicate.as_str(), "Then");
    assert_eq!(
        asserted_claims[0].args[1],
        EvalValue::Subject("s1".into()),
        "the then branch drew the first subject"
    );
    assert_eq!(left, 1, "one draw, in the branch taken");

    let (not_taken, left) = subjects_for(&State::default());
    let Outcome::Accepted {
        asserted_claims, ..
    } = not_taken
    else {
        panic!("expected Accepted");
    };
    assert_eq!(asserted_claims[0].predicate.as_str(), "Else");
    assert_eq!(left, 2, "the branch not taken drew nothing");
}

/// An `if` with no `else` and a false condition does nothing, and a
/// name a branch binds is gone after it: the next statement reads it as
/// unbound.
#[test]
fn if_without_else_is_a_no_op_and_a_branch_binding_does_not_outlive_it() {
    use ir_builder::*;
    let silent = transformation(
        "silent",
        vec!["x".into()],
        vec![
            if_(
                claim("Flag", vec![var("x")]),
                vec![assert_("Then", vec![var("x")])],
                vec![],
            ),
            assert_("After", vec![var("x")]),
        ],
    );
    let run_with_a = |t: &Transformation, state: &State| {
        let transition = trace_transition(t, vec![EvalValue::Subject("a".into())]);
        propose(
            t,
            &transition,
            state,
            &[],
            &[],
            &mut crate::kernel_tests::fresh(),
        )
    };
    let Outcome::Accepted {
        asserted_claims, ..
    } = run_with_a(&silent, &State::default()).unwrap()
    else {
        panic!("expected Accepted");
    };
    assert_eq!(asserted_claims.len(), 1);
    assert_eq!(asserted_claims[0].predicate.as_str(), "After");

    let leaky = transformation(
        "leaky",
        vec!["x".into()],
        vec![
            if_(
                claim("Flag", vec![var("x")]),
                vec![let_("inside", term(var("x")))],
                vec![],
            ),
            assert_("After", vec![var("inside")]),
        ],
    );
    let flagged = State::from_claims(vec![ClaimInstance {
        predicate: "Flag".into(),
        args: vec![EvalValue::Subject("a".into())],
    }]);
    assert!(
        matches!(run_with_a(&leaky, &flagged), Err(EvalError::UnboundVariable(v)) if v == "inside"),
        "a branch binding is scoped to its branch"
    );
}

/// A refusal or an error inside a branch stops the body there, with the
/// trace nested under the `if` entry and the outer bindings restored.
#[test]
fn propose_with_trace_nests_a_branch_and_records_which_was_taken() {
    use ir_builder::*;
    let t = transformation(
        "gated",
        vec!["x".into()],
        vec![
            if_(
                claim("Flag", vec![var("x")]),
                vec![
                    let_("inside", term(var("x"))),
                    require_named("never", claim("Missing", vec![var("inside")])),
                    assert_("Then", vec![var("x")]),
                ],
                vec![],
            ),
            assert_("After", vec![var("x")]),
        ],
    );
    let flagged = State::from_claims(vec![ClaimInstance {
        predicate: "Flag".into(),
        args: vec![EvalValue::Subject("a".into())],
    }]);
    let transition = trace_transition(&t, vec![EvalValue::Subject("a".into())]);
    let TracedProposal::Completed { outcome, trace } = propose_with_trace(
        crate::propose::Vocabulary::undeclared(),
        &t,
        &transition,
        &flagged,
        &[],
        &[],
        &mut crate::kernel_tests::fresh(),
    ) else {
        panic!("expected Completed");
    };
    assert!(
        matches!(&outcome, Outcome::Rejected { reason: RejectionReason::Require { name: Some(n), .. } } if n.as_str() == "never"),
        "the gate inside the branch refused: {outcome:?}"
    );
    assert_eq!(trace.len(), 1, "the body stopped at the if: {trace:?}");
    let TraceEntry::If {
        expression,
        taken,
        trace: inner,
    } = &trace[0]
    else {
        panic!("expected If, got {:?}", trace[0]);
    };
    assert_eq!(expression, "Flag(x)");
    assert!(taken);
    assert_eq!(inner.len(), 2, "the let and the refusing gate: {inner:?}");
    assert!(matches!(&inner[0], TraceEntry::Let { name, .. } if name.as_str() == "inside"));
    assert!(matches!(
        &inner[1],
        TraceEntry::Require {
            outcome: RequireOutcome::Rejected { .. },
            ..
        }
    ));

    // The else path on an empty state: taken is false and the trace
    // holds the branch's one statement.
    let t = transformation(
        "either",
        vec!["x".into()],
        vec![if_(
            claim("Flag", vec![var("x")]),
            vec![assert_("Then", vec![var("x")])],
            vec![assert_("Else", vec![var("x")])],
        )],
    );
    let transition = trace_transition(&t, vec![EvalValue::Subject("a".into())]);
    let TracedProposal::Completed { trace, .. } = propose_with_trace(
        crate::propose::Vocabulary::undeclared(),
        &t,
        &transition,
        &State::default(),
        &[],
        &[],
        &mut crate::kernel_tests::fresh(),
    ) else {
        panic!("expected Completed");
    };
    let TraceEntry::If {
        taken,
        trace: inner,
        ..
    } = &trace[0]
    else {
        panic!("expected If, got {:?}", trace[0]);
    };
    assert!(!taken);
    assert!(
        matches!(&inner[0], TraceEntry::Assert { claim } if claim.predicate.as_str() == "Else")
    );
}

/// A condition that cannot be evaluated is the body's error, as a
/// `require`'s would be, and a nested `if` draws only along the path
/// taken.
#[test]
fn if_propagates_a_condition_error_and_nests() {
    use ir_builder::*;
    let erring = single_stmt_transformation(
        "erring",
        vec![if_(
            eq(term(var("unbound")), term(dec("0"))),
            vec![],
            vec![],
        )],
    );
    assert!(matches!(
        run(&erring, &State::default()),
        Err(EvalError::UnboundVariable(v)) if v == "unbound"
    ));

    let nested = transformation(
        "nested",
        vec!["x".into()],
        vec![if_(
            claim("Flag", vec![var("x")]),
            vec![if_(
                claim("Inner", vec![var("x")]),
                vec![let_new_subject("a"), assert_("Deep", vec![var("a")])],
                vec![let_new_subject("b"), assert_("Shallow", vec![var("b")])],
            )],
            vec![],
        )],
    );
    let state = State::from_claims(vec![ClaimInstance {
        predicate: "Flag".into(),
        args: vec![EvalValue::Subject("a".into())],
    }]);
    let transition = trace_transition(&nested, vec![EvalValue::Subject("a".into())]);
    let mut source = vec![Subject::from("s1"), Subject::from("s2")].into_iter();
    let Outcome::Accepted {
        asserted_claims, ..
    } = propose(&nested, &transition, &state, &[], &[], &mut source).unwrap()
    else {
        panic!("expected Accepted");
    };
    assert_eq!(asserted_claims[0].predicate.as_str(), "Shallow");
    assert_eq!(asserted_claims[0].args[0], EvalValue::Subject("s1".into()));
    assert_eq!(source.count(), 1, "one draw along the path taken");
}

/// A `for` over a pattern runs once per matching claim, in the canonical
/// order of the claims' arguments, however the state was loaded: the
/// subject drawn in the second iteration goes to the same claim either
/// way. A bound variable is a key, an unbound one binds per claim and a
/// wildcard matches anything without collapsing two claims into one.
#[test]
fn for_each_runs_once_per_matching_claim_in_canonical_order_whatever_the_load_order() {
    use ir_builder::*;
    let t = transformation(
        "tag_lines",
        vec!["order".into()],
        vec![for_each(
            "Line",
            vec![var("line"), var("order"), wildcard()],
            vec![
                let_new_subject("tag"),
                assert_("Tagged", vec![var("line"), var("tag")]),
            ],
        )],
    );
    let line = |l: &str, o: &str, q: i64| ClaimInstance {
        predicate: "Line".into(),
        args: vec![
            EvalValue::Subject(l.into()),
            EvalValue::Subject(o.into()),
            EvalValue::Decimal(Decimal::new(q, 0)),
        ],
    };
    // Two lines on the order, one of them twice over (differing only in
    // the wildcarded field), and a line on another order.
    let claims = vec![
        line("b", "o", 2),
        line("a", "o", 1),
        line("c", "other", 5),
        line("a", "o", 3),
    ];
    let mut reversed = claims.clone();
    reversed.reverse();
    for state in [State::from_claims(claims), State::from_claims(reversed)] {
        let transition = trace_transition(&t, vec![EvalValue::Subject("o".into())]);
        let mut source = vec![
            Subject::from("s1"),
            Subject::from("s2"),
            Subject::from("s3"),
            Subject::from("s4"),
        ]
        .into_iter();
        let Outcome::Accepted {
            asserted_claims, ..
        } = propose(&t, &transition, &state, &[], &[], &mut source).unwrap()
        else {
            panic!("expected Accepted");
        };
        let tagged: Vec<(EvalValue, EvalValue)> = asserted_claims
            .iter()
            .map(|c| (c.args[0].clone(), c.args[1].clone()))
            .collect();
        assert_eq!(
            tagged,
            vec![
                (
                    EvalValue::Subject("a".into()),
                    EvalValue::Subject("s1".into())
                ),
                (
                    EvalValue::Subject("a".into()),
                    EvalValue::Subject("s2".into())
                ),
                (
                    EvalValue::Subject("b".into()),
                    EvalValue::Subject("s3".into())
                ),
            ],
            "a then a then b, one draw each, whatever the load order"
        );
        assert_eq!(source.count(), 1, "three iterations drew three subjects");
    }
}

/// Zero matches is zero iterations, and a name the body binds is gone
/// after the loop, as it is after a `for` over a collection.
#[test]
fn for_each_with_no_match_runs_nothing_and_its_bindings_do_not_outlive_it() {
    use ir_builder::*;
    let run = |body: Vec<Stmt>, state: &State| {
        let t = transformation("t", vec!["x".into()], body);
        let transition = trace_transition(&t, vec![EvalValue::Subject("a".into())]);
        propose(
            &t,
            &transition,
            state,
            &[],
            &[],
            &mut crate::kernel_tests::fresh(),
        )
    };
    let Outcome::Accepted {
        asserted_claims, ..
    } = run(
        vec![
            for_each(
                "Flag",
                vec![var("x"), var("why")],
                vec![assert_("Seen", vec![var("why")])],
            ),
            assert_("After", vec![var("x")]),
        ],
        &State::default(),
    )
    .unwrap()
    else {
        panic!("expected Accepted");
    };
    assert_eq!(asserted_claims.len(), 1);
    assert_eq!(asserted_claims[0].predicate.as_str(), "After");

    let flagged = State::from_claims(vec![ClaimInstance {
        predicate: "Flag".into(),
        args: vec![
            EvalValue::Subject("a".into()),
            EvalValue::Subject("w".into()),
        ],
    }]);
    let leaky = run(
        vec![
            for_each("Flag", vec![var("x"), var("why")], vec![]),
            assert_("After", vec![var("why")]),
        ],
        &flagged,
    );
    assert!(
        matches!(&leaky, Err(EvalError::UnboundVariable(v)) if v == "why"),
        "a pattern variable is scoped to the loop: {leaky:?}"
    );
}

/// Every iteration reads the pre-state: a claim one iteration admits is
/// not seen by the next. A refusal inside stops the body there, with the
/// trace listing each iteration's claim and entries up to the refusal.
#[test]
fn for_each_reads_the_pre_state_and_a_refusal_inside_is_traced_at_its_claim() {
    use ir_builder::*;
    let t = transformation(
        "mark",
        vec!["order".into()],
        vec![for_each(
            "Line",
            vec![var("line"), var("order")],
            vec![
                require_named("unmarked", not(claim("Marked", vec![var("line")]))),
                assert_("Marked", vec![var("line")]),
            ],
        )],
    );
    let line = |l: &str| ClaimInstance {
        predicate: "Line".into(),
        args: vec![EvalValue::Subject(l.into()), EvalValue::Subject("o".into())],
    };
    let marked = |l: &str| ClaimInstance {
        predicate: "Marked".into(),
        args: vec![EvalValue::Subject(l.into())],
    };
    let traced = |state: &State| {
        let transition = trace_transition(&t, vec![EvalValue::Subject("o".into())]);
        propose_with_trace(
            crate::propose::Vocabulary::undeclared(),
            &t,
            &transition,
            state,
            &[],
            &[],
            &mut crate::kernel_tests::fresh(),
        )
    };

    let TracedProposal::Completed {
        outcome: Outcome::Accepted {
            asserted_claims, ..
        },
        trace,
    } = traced(&State::from_claims(vec![line("b"), line("a")]))
    else {
        panic!("both lines are unmarked in the pre-state, so both are marked");
    };
    assert_eq!(asserted_claims.len(), 2);
    let [
        TraceEntry::ForEach {
            predicate,
            iterations,
        },
    ] = trace.as_slice()
    else {
        panic!("one for entry: {trace:?}");
    };
    assert_eq!(predicate.as_str(), "Line");
    assert_eq!(iterations.len(), 2);
    assert_eq!(iterations[0].claim, line("a"));
    assert_eq!(iterations[1].claim, line("b"));
    assert!(matches!(
        iterations[1].trace.as_slice(),
        [
            TraceEntry::Require {
                outcome: RequireOutcome::Held { .. },
                ..
            },
            TraceEntry::Assert { .. }
        ]
    ));

    let TracedProposal::Completed {
        outcome:
            Outcome::Rejected {
                reason: RejectionReason::Require { name, .. },
            },
        trace,
    } = traced(&State::from_claims(vec![line("b"), line("a"), marked("b")]))
    else {
        panic!("the marked line refuses the act");
    };
    assert_eq!(name.as_ref().map(|n| n.as_str()), Some("unmarked"));
    let [TraceEntry::ForEach { iterations, .. }] = trace.as_slice() else {
        panic!("one for entry: {trace:?}");
    };
    assert_eq!(iterations.len(), 2, "the loop stops at the refusing claim");
    assert_eq!(iterations[1].claim, line("b"));
    assert!(matches!(
        iterations[1].trace.as_slice(),
        [TraceEntry::Require {
            outcome: RequireOutcome::Rejected { .. },
            ..
        }]
    ));
}
