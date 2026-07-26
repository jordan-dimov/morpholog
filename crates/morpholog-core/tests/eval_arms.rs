//! Evaluation arms the mutation audit found unwitnessed: exact results
//! of the unit and time arithmetic, literal argument matching for the
//! time kinds, the multiple-match refusal on both candidate paths,
//! definition-projection dedup, and the missing-claim diagnosis
//! through a defined call. Each test is the minimal business behaviour
//! that makes the arm observable - a mutant flipping the arm now has a
//! named witness.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    add, assert_, claim, defined, div, duration, eq, mul, params, predicate, program, qty, require,
    sum, term, timestamp, transformation, var,
};
use morpholog_core::{
    Definition, EvalError, Outcome, Prop, State, Term, Var, propose as kernel_propose,
};
use morpholog_test_support::{propose_with_test_actor, test_transition};

/// Evaluate one requirement against an empty state with no rules: an
/// acceptance means the proposition held exactly.
fn holds(prop: Prop) {
    let t = transformation("probe", params(&[]), vec![require(prop)]);
    let outcome = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect("evaluates cleanly");
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "expected the proposition to hold exactly: {outcome:?}"
    );
}

/// Evaluate one requirement expecting a kernel error whose message
/// carries every fragment.
fn errs(prop: Prop, fragments: &[&str]) {
    let t = transformation("probe", params(&[]), vec![require(prop)]);
    let err = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect_err("expected a kernel error");
    let rendered = format!("{err:?}");
    for fragment in fragments {
        assert!(
            rendered.contains(fragment),
            "error should mention `{fragment}`: {rendered}"
        );
    }
}

#[test]
fn quantity_arithmetic_is_exact_in_every_arm() {
    // Addition adds, never subtracts or multiplies.
    holds(eq(
        add(term(qty("2.5", "t")), term(qty("0.5", "t"))),
        term(qty("3", "t")),
    ));
    // A same-unit ratio is a bare decimal, exact.
    holds(eq(
        div(term(qty("7", "t")), term(qty("2", "t"))),
        term(morpholog_core::ir_builder::dec("3.5")),
    ));
    // Scaling multiplies, never adds or divides.
    holds(eq(
        mul(
            term(qty("2.5", "t")),
            term(morpholog_core::ir_builder::dec("4")),
        ),
        term(qty("10", "t")),
    ));
    // A zero divisor is refused by name, not computed around.
    errs(
        eq(
            div(term(qty("7", "t")), term(qty("0", "t"))),
            term(morpholog_core::ir_builder::dec("0")),
        ),
        &["DivisionByZero"],
    );
    // The mirrored orders are their own arms: a scalar on the left
    // scales too, and dividing a quantity BY a scalar divides exactly
    // - with its own zero refusal.
    holds(eq(
        mul(
            term(morpholog_core::ir_builder::dec("4")),
            term(qty("2.5", "t")),
        ),
        term(qty("10", "t")),
    ));
    holds(eq(
        div(
            term(qty("10", "t")),
            term(morpholog_core::ir_builder::dec("4")),
        ),
        term(qty("2.5", "t")),
    ));
    errs(
        eq(
            div(
                term(qty("10", "t")),
                term(morpholog_core::ir_builder::dec("0")),
            ),
            term(qty("0", "t")),
        ),
        &["DivisionByZero"],
    );
}

#[test]
fn duration_arithmetic_is_exact_including_subsecond_parts() {
    // Duration addition adds.
    holds(eq(
        add(term(duration("PT1H")), term(duration("PT30M"))),
        term(duration("PT1H30M")),
    ));
    // The duration ratio is exact through the nanosecond part: the
    // subsecond component is ADDED to the whole-second count.
    holds(eq(
        div(term(duration("PT1.5S")), term(duration("PT0.5S"))),
        term(morpholog_core::ir_builder::dec("3")),
    ));
}

#[test]
fn a_malformed_timestamp_literal_is_an_error_not_a_default() {
    // Hand-built IR can carry an unparseable instant; evaluation must
    // refuse it by name, never quietly substitute some default epoch.
    errs(
        morpholog_core::ir_builder::timestamp_le(
            term(timestamp("not-a-timestamp")),
            term(timestamp("2026-07-01T12:00:00Z")),
        ),
        &["invalid timestamp", "not-a-timestamp"],
    );
}

/// Admit one claim carrying every time kind, then match it - and fail
/// to match it - with literal argument patterns.
#[test]
fn literal_time_arguments_match_exactly() {
    let p = program("literal_match")
        .predicates(vec![
            predicate("Stamp")
                .date("on")
                .timestamp("at")
                .duration("took")
                .build(),
        ])
        .transformations(vec![transformation(
            "record",
            params(&[]),
            vec![assert_(
                "Stamp",
                vec![
                    morpholog_core::ir_builder::date("2026-07-01"),
                    timestamp("2026-07-01T12:00:00Z"),
                    duration("PT1H"),
                ],
            )],
        )])
        .build();
    let t = &p.transformations[0];
    let outcome = propose_with_test_actor(t, vec![], &State::default(), &[], &[]).unwrap();
    let Outcome::Accepted {
        candidate_state, ..
    } = outcome
    else {
        panic!("record commits");
    };

    let matches_exact = transformation(
        "probe",
        params(&[]),
        vec![require(claim(
            "Stamp",
            vec![
                morpholog_core::ir_builder::date("2026-07-01"),
                timestamp("2026-07-01T12:00:00Z"),
                duration("PT1H"),
            ],
        ))],
    );
    let outcome =
        propose_with_test_actor(&matches_exact, vec![], &candidate_state, &[], &[]).unwrap();
    assert!(matches!(outcome, Outcome::Accepted { .. }));

    for (i, wrong) in [
        morpholog_core::ir_builder::date("2026-07-02"),
        timestamp("2026-07-01T12:00:01Z"),
        duration("PT2H"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut args = vec![
            morpholog_core::ir_builder::date("2026-07-01"),
            timestamp("2026-07-01T12:00:00Z"),
            duration("PT1H"),
        ];
        args[i] = wrong;
        let probe = transformation("probe", params(&[]), vec![require(claim("Stamp", args))]);
        let outcome = propose_with_test_actor(&probe, vec![], &candidate_state, &[], &[]).unwrap();
        assert!(
            matches!(outcome, Outcome::Rejected { .. }),
            "a mismatched literal in position {i} must not match"
        );
    }
}

#[test]
fn a_value_lookup_with_two_matches_is_refused_on_both_candidate_paths() {
    let p = program("multi")
        .predicates(vec![
            predicate("Reading")
                .subject("sensor")
                .decimal("level")
                .build(),
        ])
        .transformations(vec![transformation(
            "seed",
            params(&[]),
            vec![
                assert_(
                    "Reading",
                    vec![
                        morpholog_core::ir_builder::subj("s1"),
                        morpholog_core::ir_builder::dec("1"),
                    ],
                ),
                assert_(
                    "Reading",
                    vec![
                        morpholog_core::ir_builder::subj("s1"),
                        morpholog_core::ir_builder::dec("2"),
                    ],
                ),
            ],
        )])
        .build();
    let t = &p.transformations[0];
    let Outcome::Accepted {
        candidate_state, ..
    } = propose_with_test_actor(t, vec![], &State::default(), &[], &[]).unwrap()
    else {
        panic!("seed commits");
    };

    // Indexed path: the subject narrows the candidates.
    let by_subject = transformation(
        "probe",
        params(&[]),
        vec![require(eq(
            morpholog_core::ir_builder::value_of(
                "Reading",
                vec![morpholog_core::ir_builder::subj("s1"), Term::Wildcard],
            ),
            term(morpholog_core::ir_builder::dec("1")),
        ))],
    );
    let err = propose_with_test_actor(&by_subject, vec![], &candidate_state, &[], &[]).unwrap_err();
    assert!(matches!(err, EvalError::ValueOfMultipleMatches(_)));

    // Unindexed path: wildcards only.
    let all_wild = transformation(
        "probe",
        params(&[]),
        vec![require(eq(
            morpholog_core::ir_builder::value_of("Reading", vec![Term::Wildcard, Term::Wildcard]),
            term(morpholog_core::ir_builder::dec("1")),
        ))],
    );
    let err = propose_with_test_actor(&all_wild, vec![], &candidate_state, &[], &[]).unwrap_err();
    assert!(matches!(err, EvalError::ValueOfMultipleMatches(_)));
}

#[test]
fn a_defined_call_projects_each_witness_once() {
    // Two claims share the projected binding: the call yields the
    // distinct projection once, so counting the call counts
    // projections, never internal multiplicity.
    let definition = Definition {
        origin: morpholog_core::DefinitionOrigin::Authored,
        name: "sensed".into(),
        parameters: vec![Var::from("s")],
        body: claim("Reading", vec![var("s"), Term::Wildcard]),
    };
    let p = program("dedup")
        .predicates(vec![
            predicate("Reading")
                .subject("sensor")
                .decimal("level")
                .build(),
        ])
        .transformations(vec![
            transformation(
                "seed",
                params(&[]),
                vec![
                    assert_(
                        "Reading",
                        vec![
                            morpholog_core::ir_builder::subj("s1"),
                            morpholog_core::ir_builder::dec("1"),
                        ],
                    ),
                    assert_(
                        "Reading",
                        vec![
                            morpholog_core::ir_builder::subj("s1"),
                            morpholog_core::ir_builder::dec("2"),
                        ],
                    ),
                ],
            ),
            transformation(
                "count",
                params(&[]),
                vec![require(eq(
                    sum(
                        morpholog_core::ir_builder::dec("1"),
                        defined("sensed", vec![Term::Wildcard]),
                    ),
                    term(morpholog_core::ir_builder::dec("1")),
                ))],
            ),
        ])
        .build();
    let Outcome::Accepted {
        candidate_state, ..
    } = propose_with_test_actor(
        &p.transformations[0],
        vec![],
        &State::default(),
        &[],
        &p.definitions,
    )
    .unwrap()
    else {
        panic!("seed commits");
    };
    let definitions = vec![definition];
    let outcome = propose_with_test_actor(
        &p.transformations[1],
        vec![],
        &candidate_state,
        &[],
        &definitions,
    )
    .unwrap();
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "one distinct projection, counted once: {outcome:?}"
    );

    // And two DISTINCT projections both count: dedup removes internal
    // multiplicity, never distinct witnesses.
    let extra = transformation(
        "seed_more",
        params(&[]),
        vec![assert_(
            "Reading",
            vec![
                morpholog_core::ir_builder::subj("s2"),
                morpholog_core::ir_builder::dec("3"),
            ],
        )],
    );
    let Outcome::Accepted {
        candidate_state: wider,
        ..
    } = propose_with_test_actor(&extra, vec![], &candidate_state, &[], &definitions).unwrap()
    else {
        panic!("seed_more commits");
    };
    let count_two = transformation(
        "count_two",
        params(&[]),
        vec![require(eq(
            sum(
                morpholog_core::ir_builder::dec("1"),
                defined("sensed", vec![var("s")]),
            ),
            term(morpholog_core::ir_builder::dec("2")),
        ))],
    );
    let outcome = propose_with_test_actor(&count_two, vec![], &wider, &[], &definitions).unwrap();
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "two distinct projections both count: {outcome:?}"
    );
}

#[test]
fn the_missing_claim_diagnosis_descends_a_defined_gate() {
    // A gate that is a defined call over an absent predicate: the
    // explanation names the predicate inside the definition body and
    // the transformation that could supply it.
    let p = program("explain_defined")
        .predicates(vec![predicate("Approved").subject("item").build()])
        .transformations(vec![
            transformation(
                "approve",
                params(&["item"]),
                vec![assert_("Approved", vec![var("item")])],
            ),
            transformation(
                "ship",
                params(&["item"]),
                vec![require(defined("is_approved", vec![var("item")]))],
            ),
        ])
        .build();
    let mut p = p;
    p.definitions = vec![Definition {
        origin: morpholog_core::DefinitionOrigin::Authored,
        name: "is_approved".into(),
        parameters: vec![Var::from("i")],
        body: claim("Approved", vec![var("i")]),
    }];

    let ship = p.transformations[1].clone();
    let transition = test_transition(&ship, vec![morpholog_test_support::subj("box_1")]);
    // The kernel refuses first (the gate fails), then the explanation
    // diagnoses the same snapshot.
    let outcome =
        kernel_propose(&ship, &transition, &State::default(), &[], &p.definitions).unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
    let explanation = morpholog_core::explain(&p, &transition, &State::default());
    let rendered = serde_json::to_string(&explanation).unwrap();
    assert!(
        rendered.contains("Approved") && rendered.contains("approve"),
        "the diagnosis reaches through the defined call: {rendered}"
    );
}

#[test]
fn literal_time_patterns_govern_retraction_too() {
    // Pattern-based retraction matches stored values against literal
    // arguments through the same parsed comparisons as reads: the
    // exact literal removes the claim, a near-miss in any position
    // fails the retraction of that specific claim.
    use morpholog_core::ir_builder::retract;
    let stamp_args = || {
        vec![
            morpholog_core::ir_builder::date("2026-07-01"),
            timestamp("2026-07-01T12:00:00Z"),
            duration("PT1H"),
        ]
    };
    let p = program("retract_literals")
        .predicates(vec![
            predicate("Stamp")
                .date("on")
                .timestamp("at")
                .duration("took")
                .build(),
        ])
        .transformations(vec![transformation(
            "record",
            params(&[]),
            vec![assert_("Stamp", stamp_args())],
        )])
        .build();
    let Outcome::Accepted {
        candidate_state, ..
    } = propose_with_test_actor(&p.transformations[0], vec![], &State::default(), &[], &[])
        .unwrap()
    else {
        panic!("record commits");
    };

    for (i, wrong) in [
        morpholog_core::ir_builder::date("2026-07-02"),
        timestamp("2026-07-01T12:00:01Z"),
        duration("PT2H"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut args = stamp_args();
        args[i] = wrong;
        let near_miss = transformation("undo", params(&[]), vec![retract("Stamp", args)]);
        let outcome =
            propose_with_test_actor(&near_miss, vec![], &candidate_state, &[], &[]).unwrap();
        let Outcome::Accepted {
            candidate_state: after,
            ..
        } = outcome
        else {
            panic!("zero-match retraction is an idempotent no-op");
        };
        assert_eq!(
            after.claims().len(),
            1,
            "a near-miss literal in position {i} must not remove the claim"
        );
    }

    let exact = transformation("undo", params(&[]), vec![retract("Stamp", stamp_args())]);
    let outcome = propose_with_test_actor(&exact, vec![], &candidate_state, &[], &[]).unwrap();
    let Outcome::Accepted {
        candidate_state: after,
        ..
    } = outcome
    else {
        panic!("the exact literal retracts");
    };
    assert!(after.claims().is_empty(), "the stamp is gone");
}

#[test]
fn a_value_lookup_matching_one_of_two_claims_reads_it() {
    // Two claims under the predicate, ONE matching the pattern: the
    // lookup succeeds with that claim's value - a matcher that treated
    // any same-shape claim as a match would refuse with multiple
    // matches instead.
    let p = program("single_of_two")
        .predicates(vec![
            predicate("Reading")
                .subject("sensor")
                .decimal("level")
                .build(),
        ])
        .transformations(vec![transformation(
            "seed",
            params(&[]),
            vec![
                assert_(
                    "Reading",
                    vec![
                        morpholog_core::ir_builder::subj("s1"),
                        morpholog_core::ir_builder::dec("1"),
                    ],
                ),
                assert_(
                    "Reading",
                    vec![
                        morpholog_core::ir_builder::subj("s2"),
                        morpholog_core::ir_builder::dec("2"),
                    ],
                ),
            ],
        )])
        .build();
    let Outcome::Accepted {
        candidate_state, ..
    } = propose_with_test_actor(&p.transformations[0], vec![], &State::default(), &[], &[])
        .unwrap()
    else {
        panic!("seed commits");
    };
    let probe = transformation(
        "probe",
        params(&[]),
        vec![require(eq(
            morpholog_core::ir_builder::value_of(
                "Reading",
                vec![morpholog_core::ir_builder::subj("s1"), Term::Wildcard],
            ),
            term(morpholog_core::ir_builder::dec("1")),
        ))],
    );
    let outcome = propose_with_test_actor(&probe, vec![], &candidate_state, &[], &[]).unwrap();
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "exactly one claim matches, and its value reads: {outcome:?}"
    );
}

#[test]
fn a_repeated_call_variable_must_project_consistently() {
    // `pair(z, z)`: a witness whose two projected arguments differ is
    // discarded - the repeated variable is one binding, not two.
    let definition = Definition {
        origin: morpholog_core::DefinitionOrigin::Authored,
        name: "pair".into(),
        parameters: vec![Var::from("p"), Var::from("q")],
        body: claim("Link", vec![var("p"), var("q")]),
    };
    let p = program("consistency")
        .predicates(vec![predicate("Link").subject("a").subject("b").build()])
        .transformations(vec![
            transformation(
                "seed",
                params(&[]),
                vec![
                    assert_(
                        "Link",
                        vec![
                            morpholog_core::ir_builder::subj("x1"),
                            morpholog_core::ir_builder::subj("x2"),
                        ],
                    ),
                    assert_(
                        "Link",
                        vec![
                            morpholog_core::ir_builder::subj("x3"),
                            morpholog_core::ir_builder::subj("x3"),
                        ],
                    ),
                ],
            ),
            transformation(
                "count_selfpairs",
                params(&[]),
                vec![require(eq(
                    sum(
                        morpholog_core::ir_builder::dec("1"),
                        defined("pair", vec![var("z"), var("z")]),
                    ),
                    term(morpholog_core::ir_builder::dec("1")),
                ))],
            ),
        ])
        .build();
    let definitions = vec![definition];
    let Outcome::Accepted {
        candidate_state, ..
    } = propose_with_test_actor(
        &p.transformations[0],
        vec![],
        &State::default(),
        &[],
        &definitions,
    )
    .unwrap()
    else {
        panic!("seed commits");
    };
    let outcome = propose_with_test_actor(
        &p.transformations[1],
        vec![],
        &candidate_state,
        &[],
        &definitions,
    )
    .unwrap();
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "only the self-pair projects: {outcome:?}"
    );
}

#[test]
fn a_lookup_excludes_a_bucket_mate_the_pattern_rules_out() {
    // Two claims share the indexed subject; the pattern's second
    // subject rules one out. The lookup must read the survivor - a
    // matcher that admitted any same-shape candidate would refuse
    // with multiple matches.
    let p = program("bucket_mate")
        .predicates(vec![
            predicate("Reading")
                .subject("sensor")
                .subject("channel")
                .decimal("level")
                .build(),
        ])
        .transformations(vec![transformation(
            "seed",
            params(&[]),
            vec![
                assert_(
                    "Reading",
                    vec![
                        morpholog_core::ir_builder::subj("s1"),
                        morpholog_core::ir_builder::subj("c1"),
                        morpholog_core::ir_builder::dec("1"),
                    ],
                ),
                assert_(
                    "Reading",
                    vec![
                        morpholog_core::ir_builder::subj("s1"),
                        morpholog_core::ir_builder::subj("c2"),
                        morpholog_core::ir_builder::dec("2"),
                    ],
                ),
            ],
        )])
        .build();
    let Outcome::Accepted {
        candidate_state, ..
    } = propose_with_test_actor(&p.transformations[0], vec![], &State::default(), &[], &[])
        .unwrap()
    else {
        panic!("seed commits");
    };
    let probe = transformation(
        "probe",
        params(&[]),
        vec![require(eq(
            morpholog_core::ir_builder::value_of(
                "Reading",
                vec![
                    morpholog_core::ir_builder::subj("s1"),
                    morpholog_core::ir_builder::subj("c1"),
                    Term::Wildcard,
                ],
            ),
            term(morpholog_core::ir_builder::dec("1")),
        ))],
    );
    let outcome = propose_with_test_actor(&probe, vec![], &candidate_state, &[], &[]).unwrap();
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "the pattern rules out the bucket mate: {outcome:?}"
    );
}
