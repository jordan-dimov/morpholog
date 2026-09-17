//! The failure walk: which sub-expression a refusal is pinned to.

use super::*;

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
