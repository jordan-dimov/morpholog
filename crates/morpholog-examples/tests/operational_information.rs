//! Integration tests for the operational-information example
//! (`examples/20_operational_information/`).
//!
//! The expression-valued sum forcing example: an untrusted harness
//! files exact expected-loss certificates and the record recomputes
//! every figure as `sum(probability * loss | ...)`. These tests walk
//! the binary parity (XOR) experiment end to end and pin the refusal
//! surface: a wrong contribution, a beaten action, a premature or
//! wrong total, a wrong decision value, an incomplete seal, a sighted
//! baseline, and a nominated joint that invents or forgets member
//! information.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, dec_str, has_claim, subj, test_actor};
use morpholog_core::{EvalError, EvalValue, Outcome, RejectionReason, State, enumerate_derived};
use morpholog_examples::operational_information as op;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&op::program()))
}

fn accept(state: State, t: &morpholog_core::Transformation, args: Vec<EvalValue>) -> State {
    ex().must_accept(t, args, state)
}

fn rejected_by(outcome: &Result<Outcome, EvalError>, rule: &str) -> bool {
    matches!(
        outcome,
        Ok(Outcome::Rejected {
            reason: RejectionReason::Invariant { name, .. },
            ..
        }) if name.as_str() == rule
    )
}

fn gate_rejected(outcome: &Result<Outcome, EvalError>) -> bool {
    matches!(
        outcome,
        Ok(Outcome::Rejected {
            reason: RejectionReason::Require { .. },
            ..
        })
    )
}

/// The four uniform cases of `W = A xor B`, worlds `w0`/`w1`, with
/// every observation mapping and every loss declared - stopping just
/// short of the seal so refusal tests can perturb the construction.
fn xor_unsealed() -> State {
    let mut s = State::default();
    s = accept(s, &op::create_experiment(), vec![subj("exp"), subj("none")]);
    for c in ["source_a", "source_b", "joint_ab"] {
        s = accept(s, &op::declare_coalition(), vec![subj("exp"), subj(c)]);
    }
    s = accept(
        s,
        &op::declare_pair(),
        vec![
            subj("exp"),
            subj("source_a"),
            subj("source_b"),
            subj("joint_ab"),
        ],
    );
    for a in ["predict_w0", "predict_w1"] {
        s = accept(s, &op::declare_action(), vec![subj("exp"), subj(a)]);
    }
    // (case, world): c00 and c11 have even parity, c01 and c10 odd.
    for (c, w) in [("c00", "w0"), ("c01", "w1"), ("c10", "w1"), ("c11", "w0")] {
        s = accept(
            s,
            &op::declare_case(),
            vec![subj("exp"), subj(c), subj(w), dec_str("0.25")],
        );
    }
    let mappings = [
        ("none", "c00", "all"),
        ("none", "c01", "all"),
        ("none", "c10", "all"),
        ("none", "c11", "all"),
        ("source_a", "c00", "a0"),
        ("source_a", "c01", "a0"),
        ("source_a", "c10", "a1"),
        ("source_a", "c11", "a1"),
        ("source_b", "c00", "b0"),
        ("source_b", "c01", "b1"),
        ("source_b", "c10", "b0"),
        ("source_b", "c11", "b1"),
        ("joint_ab", "c00", "j00"),
        ("joint_ab", "c01", "j01"),
        ("joint_ab", "c10", "j10"),
        ("joint_ab", "c11", "j11"),
    ];
    for (coalition, case, obs) in mappings {
        s = accept(
            s,
            &op::map_observation(),
            vec![subj("exp"), subj(coalition), subj(case), subj(obs)],
        );
    }
    for (w, a, l) in [
        ("w0", "predict_w0", "0"),
        ("w0", "predict_w1", "1"),
        ("w1", "predict_w0", "1"),
        ("w1", "predict_w1", "0"),
    ] {
        s = accept(
            s,
            &op::declare_loss(),
            vec![subj("exp"), subj(w), subj(a), dec_str(l)],
        );
    }
    s
}

fn xor_sealed() -> State {
    accept(xor_unsealed(), &op::seal_experiment(), vec![subj("exp")])
}

/// Every coalition's optimal choices. Single-source observations are
/// exact ties at 0.25 either way (that is the parity point); the
/// joint predicts the world and contributes zero.
fn choices() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    vec![
        ("none", "all", "predict_w0", "0.5"),
        ("source_a", "a0", "predict_w0", "0.25"),
        ("source_a", "a1", "predict_w0", "0.25"),
        ("source_b", "b0", "predict_w0", "0.25"),
        ("source_b", "b1", "predict_w0", "0.25"),
        ("joint_ab", "j00", "predict_w0", "0"),
        ("joint_ab", "j01", "predict_w1", "0"),
        ("joint_ab", "j10", "predict_w1", "0"),
        ("joint_ab", "j11", "predict_w0", "0"),
    ]
}

fn xor_certified() -> State {
    let mut s = xor_sealed();
    for (coalition, obs, action, risk) in choices() {
        s = accept(
            s,
            &op::submit_choice(),
            vec![
                subj("exp"),
                subj(coalition),
                subj(obs),
                subj(action),
                dec_str(risk),
            ],
        );
    }
    for (coalition, risk) in [
        ("none", "0.5"),
        ("source_a", "0.5"),
        ("source_b", "0.5"),
        ("joint_ab", "0"),
    ] {
        s = accept(
            s,
            &op::certify_coalition_risk(),
            vec![subj("exp"), subj(coalition), dec_str(risk)],
        );
    }
    for (coalition, gain) in [
        ("none", "0"),
        ("source_a", "0"),
        ("source_b", "0"),
        ("joint_ab", "0.5"),
    ] {
        s = accept(
            s,
            &op::certify_decision_value(),
            vec![subj("exp"), subj(coalition), dec_str(gain)],
        );
    }
    s
}

#[test]
fn the_xor_experiment_certifies_end_to_end() {
    let state = xor_certified();
    assert!(has_claim(
        &state,
        "DecisionValue",
        &[subj("exp"), subj("joint_ab"), dec_str("0.5")]
    ));

    // The headline read: each source alone is worth nothing, the pair
    // is worth the whole half - so the additive excess IS the joint
    // value.
    let rows = enumerate_derived(&op::pair_synergy(), &state, &[]).expect("synergy enumerates");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].args,
        vec![
            subj("exp"),
            subj("source_a"),
            subj("source_b"),
            subj("joint_ab"),
            dec_str("0.5")
        ]
    );

    // The summary recomputes each gain from certified risks.
    let rows =
        enumerate_derived(&op::information_summary(), &state, &[]).expect("summary enumerates");
    assert_eq!(rows.len(), 4);
    for row in rows {
        let coalition = row.args[1].clone();
        let gain = row.args[3].clone();
        let expected = if coalition == subj("joint_ab") {
            dec_str("0.5")
        } else {
            dec_str("0")
        };
        assert_eq!(gain, expected, "gain for {coalition:?}");
    }
}

#[test]
fn a_wrong_weighted_contribution_is_refused() {
    // 0.3 where the recompute over a0's two cases is 0.25.
    let outcome = ex().propose_as(
        &op::submit_choice(),
        vec![
            subj("exp"),
            subj("source_a"),
            subj("a0"),
            subj("predict_w0"),
            dec_str("0.3"),
        ],
        test_actor(),
        &xor_sealed(),
    );
    assert!(
        rejected_by(&outcome, "choice_risk_is_the_exact_recompute"),
        "got: {outcome:?}"
    );
}

#[test]
fn a_correctly_recomputed_but_beaten_action_is_refused() {
    // At j00 the world is w0. predict_w1's contribution really is
    // 0.25 - the arithmetic is honest - but predict_w0 achieves 0, so
    // the certificate is refused on optimality, not on the recompute.
    let outcome = ex().propose_as(
        &op::submit_choice(),
        vec![
            subj("exp"),
            subj("joint_ab"),
            subj("j00"),
            subj("predict_w1"),
            dec_str("0.25"),
        ],
        test_actor(),
        &xor_sealed(),
    );
    assert!(
        rejected_by(&outcome, "chosen_action_is_bayes_optimal"),
        "got: {outcome:?}"
    );
}

#[test]
fn a_total_before_every_choice_is_refused() {
    // Only one of source_a's two observations has a choice; the
    // completeness gate on the total says no.
    let mut s = xor_sealed();
    s = accept(
        s,
        &op::submit_choice(),
        vec![
            subj("exp"),
            subj("source_a"),
            subj("a0"),
            subj("predict_w0"),
            dec_str("0.25"),
        ],
    );
    let outcome = ex().propose_as(
        &op::certify_coalition_risk(),
        vec![subj("exp"), subj("source_a"), dec_str("0.25")],
        test_actor(),
        &s,
    );
    assert!(gate_rejected(&outcome), "got: {outcome:?}");
}

#[test]
fn a_wrong_coalition_total_is_refused() {
    let mut s = xor_sealed();
    for (coalition, obs, action, risk) in choices() {
        s = accept(
            s,
            &op::submit_choice(),
            vec![
                subj("exp"),
                subj(coalition),
                subj(obs),
                subj(action),
                dec_str(risk),
            ],
        );
    }
    let outcome = ex().propose_as(
        &op::certify_coalition_risk(),
        vec![subj("exp"), subj("none"), dec_str("0.4")],
        test_actor(),
        &s,
    );
    assert!(
        rejected_by(&outcome, "coalition_risk_is_the_sum_of_its_choices"),
        "got: {outcome:?}"
    );
}

#[test]
fn a_wrong_decision_value_is_refused() {
    let mut s = xor_sealed();
    for (coalition, obs, action, risk) in choices() {
        s = accept(
            s,
            &op::submit_choice(),
            vec![
                subj("exp"),
                subj(coalition),
                subj(obs),
                subj(action),
                dec_str(risk),
            ],
        );
    }
    for (coalition, risk) in [("none", "0.5"), ("joint_ab", "0")] {
        s = accept(
            s,
            &op::certify_coalition_risk(),
            vec![subj("exp"), subj(coalition), dec_str(risk)],
        );
    }
    let outcome = ex().propose_as(
        &op::certify_decision_value(),
        vec![subj("exp"), subj("joint_ab"), dec_str("0.4")],
        test_actor(),
        &s,
    );
    assert!(
        rejected_by(&outcome, "decision_value_is_the_risk_given_up"),
        "got: {outcome:?}"
    );
}

#[test]
fn sealing_with_probabilities_short_of_one_is_refused() {
    // Same construction, one probability at 0.2: total 0.95.
    let mut s = State::default();
    s = accept(s, &op::create_experiment(), vec![subj("exp"), subj("none")]);
    s = accept(
        s,
        &op::declare_action(),
        vec![subj("exp"), subj("predict_w0")],
    );
    for (c, w, p) in [
        ("c00", "w0", "0.25"),
        ("c01", "w1", "0.25"),
        ("c10", "w1", "0.25"),
        ("c11", "w0", "0.2"),
    ] {
        s = accept(
            s,
            &op::declare_case(),
            vec![subj("exp"), subj(c), subj(w), dec_str(p)],
        );
    }
    for c in ["c00", "c01", "c10", "c11"] {
        s = accept(
            s,
            &op::map_observation(),
            vec![subj("exp"), subj("none"), subj(c), subj("all")],
        );
    }
    for (w, l) in [("w0", "0"), ("w1", "1")] {
        s = accept(
            s,
            &op::declare_loss(),
            vec![subj("exp"), subj(w), subj("predict_w0"), dec_str(l)],
        );
    }
    let outcome = ex().propose_as(&op::seal_experiment(), vec![subj("exp")], test_actor(), &s);
    assert!(
        rejected_by(&outcome, "sealed_experiment_has_unit_probability"),
        "got: {outcome:?}"
    );
}

#[test]
fn sealing_with_an_unobserved_case_is_refused() {
    // Remove one of the joint coalition's mappings by rebuilding
    // without it: the seal names the totality rule.
    let mut s = State::default();
    s = accept(s, &op::create_experiment(), vec![subj("exp"), subj("none")]);
    s = accept(
        s,
        &op::declare_action(),
        vec![subj("exp"), subj("predict_w0")],
    );
    for (c, w) in [("c00", "w0"), ("c01", "w1")] {
        s = accept(
            s,
            &op::declare_case(),
            vec![subj("exp"), subj(c), subj(w), dec_str("0.5")],
        );
    }
    // Only c00 is mapped.
    s = accept(
        s,
        &op::map_observation(),
        vec![subj("exp"), subj("none"), subj("c00"), subj("all")],
    );
    for (w, l) in [("w0", "0"), ("w1", "1")] {
        s = accept(
            s,
            &op::declare_loss(),
            vec![subj("exp"), subj(w), subj("predict_w0"), dec_str(l)],
        );
    }
    let outcome = ex().propose_as(&op::seal_experiment(), vec![subj("exp")], test_actor(), &s);
    assert!(
        rejected_by(&outcome, "sealed_coalitions_observe_every_case"),
        "got: {outcome:?}"
    );
}

#[test]
fn sealing_with_an_unpriced_outcome_is_refused() {
    let mut s = State::default();
    s = accept(s, &op::create_experiment(), vec![subj("exp"), subj("none")]);
    s = accept(
        s,
        &op::declare_action(),
        vec![subj("exp"), subj("predict_w0")],
    );
    for (c, w) in [("c00", "w0"), ("c01", "w1")] {
        s = accept(
            s,
            &op::declare_case(),
            vec![subj("exp"), subj(c), subj(w), dec_str("0.5")],
        );
        s = accept(
            s,
            &op::map_observation(),
            vec![subj("exp"), subj("none"), subj(c), subj("all")],
        );
    }
    // w1 has no price for the declared action.
    s = accept(
        s,
        &op::declare_loss(),
        vec![subj("exp"), subj("w0"), subj("predict_w0"), dec_str("0")],
    );
    let outcome = ex().propose_as(&op::seal_experiment(), vec![subj("exp")], test_actor(), &s);
    assert!(
        rejected_by(&outcome, "sealed_losses_price_every_outcome"),
        "got: {outcome:?}"
    );
}

#[test]
fn sealing_with_no_actions_is_refused() {
    let mut s = State::default();
    s = accept(s, &op::create_experiment(), vec![subj("exp"), subj("none")]);
    for (c, w) in [("c00", "w0"), ("c01", "w1")] {
        s = accept(
            s,
            &op::declare_case(),
            vec![subj("exp"), subj(c), subj(w), dec_str("0.5")],
        );
        s = accept(
            s,
            &op::map_observation(),
            vec![subj("exp"), subj("none"), subj(c), subj("all")],
        );
    }
    let outcome = ex().propose_as(&op::seal_experiment(), vec![subj("exp")], test_actor(), &s);
    assert!(
        rejected_by(&outcome, "sealed_experiment_has_an_action"),
        "got: {outcome:?}"
    );
}

#[test]
fn construction_after_the_seal_is_refused() {
    let outcome = ex().propose_as(
        &op::declare_case(),
        vec![subj("exp"), subj("c_extra"), subj("w0"), dec_str("0")],
        test_actor(),
        &xor_sealed(),
    );
    assert!(gate_rejected(&outcome), "got: {outcome:?}");
}

#[test]
fn a_baseline_that_distinguishes_two_cases_is_refused() {
    // The blind-reference guarantee: the moment the baseline's second
    // mapping disagrees with its first, the admission is refused.
    let mut s = State::default();
    s = accept(s, &op::create_experiment(), vec![subj("exp"), subj("none")]);
    for (c, w) in [("c00", "w0"), ("c01", "w1")] {
        s = accept(
            s,
            &op::declare_case(),
            vec![subj("exp"), subj(c), subj(w), dec_str("0.5")],
        );
    }
    s = accept(
        s,
        &op::map_observation(),
        vec![subj("exp"), subj("none"), subj("c00"), subj("all")],
    );
    let outcome = ex().propose_as(
        &op::map_observation(),
        vec![subj("exp"), subj("none"), subj("c01"), subj("peeked")],
        test_actor(),
        &s,
    );
    assert!(
        rejected_by(&outcome, "baseline_observes_nothing"),
        "got: {outcome:?}"
    );
}

/// A two-case rig for the join-governance refusals: both members'
/// mappings are laid down first, then the joint's second mapping
/// completes or breaks the pattern.
fn pair_rig(left_obs: [&str; 2], right_obs: [&str; 2]) -> State {
    let mut s = State::default();
    s = accept(s, &op::create_experiment(), vec![subj("exp"), subj("none")]);
    for c in ["left", "right", "joint"] {
        s = accept(s, &op::declare_coalition(), vec![subj("exp"), subj(c)]);
    }
    s = accept(
        s,
        &op::declare_pair(),
        vec![subj("exp"), subj("left"), subj("right"), subj("joint")],
    );
    for (c, w) in [("c1", "w0"), ("c2", "w1")] {
        s = accept(
            s,
            &op::declare_case(),
            vec![subj("exp"), subj(c), subj(w), dec_str("0.5")],
        );
    }
    for (i, c) in ["c1", "c2"].into_iter().enumerate() {
        s = accept(
            s,
            &op::map_observation(),
            vec![subj("exp"), subj("left"), subj(c), subj(left_obs[i])],
        );
        s = accept(
            s,
            &op::map_observation(),
            vec![subj("exp"), subj("right"), subj(c), subj(right_obs[i])],
        );
    }
    s
}

#[test]
fn a_joint_that_invents_information_is_refused() {
    // Neither member separates the two cases, so a joint that does is
    // consulting an oracle the pair does not have.
    let mut s = pair_rig(["l0", "l0"], ["r0", "r0"]);
    s = accept(
        s,
        &op::map_observation(),
        vec![subj("exp"), subj("joint"), subj("c1"), subj("j0")],
    );
    let outcome = ex().propose_as(
        &op::map_observation(),
        vec![subj("exp"), subj("joint"), subj("c2"), subj("j1")],
        test_actor(),
        &s,
    );
    assert!(
        rejected_by(&outcome, "joint_does_not_invent_information"),
        "got: {outcome:?}"
    );
}

#[test]
fn a_joint_that_forgets_member_information_is_refused() {
    // The left member separates the cases; a joint that lumps them
    // has thrown a member's information away.
    let mut s = pair_rig(["l0", "l1"], ["r0", "r0"]);
    s = accept(
        s,
        &op::map_observation(),
        vec![subj("exp"), subj("joint"), subj("c1"), subj("j0")],
    );
    let outcome = ex().propose_as(
        &op::map_observation(),
        vec![subj("exp"), subj("joint"), subj("c2"), subj("j0")],
        test_actor(),
        &s,
    );
    assert!(
        rejected_by(&outcome, "joint_preserves_member_information"),
        "got: {outcome:?}"
    );
}

#[test]
fn pair_synergy_is_empty_until_every_value_is_certified() {
    // A half-certified experiment yields no synergy row - lawfully
    // empty, not an evaluation error.
    let rows = enumerate_derived(&op::pair_synergy(), &xor_sealed(), &[])
        .expect("an uncertified experiment reads as empty, not as an error");
    assert!(rows.is_empty());
}
