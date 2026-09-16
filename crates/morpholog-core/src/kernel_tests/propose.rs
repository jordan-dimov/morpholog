//! Proposals: `bind`, the pinned error and reason renderings, and the
//! trace a proposal leaves on every path.

use super::*;

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

// ============================================================
// Expression failure-walk.
//
// When `require` or `bind_one` rejects, the trace's
// `failing_sub_expression` field carries the most specific
// sub-expression responsible. These tests pin which expression
// shapes drill in and which return None.
// ============================================================
