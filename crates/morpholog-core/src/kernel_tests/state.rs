//! The layered state is invisible: every physical layering of one
//! logical history answers exactly as the naive rebuild does.

use proptest::prelude::*;

use super::*;
use crate::ir::PredicateName;
use crate::state::{Candidate, StateRef};

/// The claims a history draws from: few enough that duplicate
/// admissions, retractions of absent claims, and retract-then-readmit
/// across steps all happen constantly.
fn claim_strategy() -> impl Strategy<Value = ClaimInstance> {
    (0..3u8, 0..4u8, 0..3u8).prop_map(|(p, a, b)| ClaimInstance {
        predicate: ["P", "Q", "R"][p as usize].into(),
        args: vec![
            EvalValue::Subject(format!("s{a}").into()),
            EvalValue::Decimal(rust_decimal::Decimal::from(b)),
        ],
    })
}

/// One act: what it retracts, then what it admits.
fn delta_strategy() -> impl Strategy<Value = (Vec<ClaimInstance>, Vec<ClaimInstance>)> {
    (
        proptest::collection::vec(claim_strategy(), 0..3),
        proptest::collection::vec(claim_strategy(), 0..3),
    )
}

/// The plain algorithm, as the oracle: copy, drop every retracted
/// claim, append each admitted claim not already present.
fn naive(
    pre: &[ClaimInstance],
    asserted: &[ClaimInstance],
    retracted: &[ClaimInstance],
) -> Vec<ClaimInstance> {
    let mut claims: Vec<ClaimInstance> = pre
        .iter()
        .filter(|c| !retracted.contains(c))
        .cloned()
        .collect();
    for a in asserted {
        if !claims.contains(a) {
            claims.push(a.clone());
        }
    }
    claims
}

/// The ground-argument buckets the evaluator narrows by, read through
/// the state and predicted from the plain claim list. A missing bucket
/// and an empty one read the same: no claim to check.
fn buckets(state: &State) -> Vec<(String, Vec<ClaimInstance>)> {
    let mut out = Vec::new();
    for predicate in ["P", "Q", "R"] {
        let name: PredicateName = predicate.into();
        for pos in 0..2 {
            for value in [
                EvalValue::Subject("s0".into()),
                EvalValue::Subject("s1".into()),
                EvalValue::Decimal(rust_decimal::Decimal::from(1)),
            ] {
                let claims = state
                    .claim_candidates(&name, pos, &value)
                    .map(|bucket| bucket.iter().cloned().collect())
                    .unwrap_or_default();
                out.push((format!("{predicate}[{pos}]={value:?}"), claims));
            }
        }
    }
    out
}

fn expected_buckets(claims: &[ClaimInstance]) -> Vec<(String, Vec<ClaimInstance>)> {
    let mut out = Vec::new();
    for predicate in ["P", "Q", "R"] {
        for pos in 0..2 {
            for value in [
                EvalValue::Subject("s0".into()),
                EvalValue::Subject("s1".into()),
                EvalValue::Decimal(rust_decimal::Decimal::from(1)),
            ] {
                let matching = claims
                    .iter()
                    .filter(|c| c.predicate.as_str() == predicate && c.args[pos] == value)
                    .cloned()
                    .collect();
                out.push((format!("{predicate}[{pos}]={value:?}"), matching));
            }
        }
    }
    out
}

/// How a test advances one of its layerings: the ordinary upkeep at a
/// given threshold, or a replay's at given dead and overlay floors.
#[derive(Clone, Copy)]
enum Upkeep {
    Threshold(usize),
    Replay(usize, usize),
}

proptest! {
    #[test]
    fn every_layering_reads_as_the_naive_rebuild(
        seed in proptest::collection::vec(claim_strategy(), 0..12),
        history in proptest::collection::vec(delta_strategy(), 1..12),
    ) {
        let mut naive_claims = naive(&[], &seed, &[]);
        let base = State::from_claims(naive_claims.clone());
        // Compaction on every change, every few, and never, then a
        // replay's upkeep at floors this history crosses constantly: one
        // logical history, six physical ones.
        let mut layered = [
            base.clone(),
            base.clone(),
            base.clone(),
            base.clone(),
            base.clone(),
            base,
        ];
        let upkeeps = [
            Upkeep::Threshold(0),
            Upkeep::Threshold(2),
            Upkeep::Threshold(7),
            Upkeep::Threshold(usize::MAX),
            Upkeep::Replay(0, 0),
            Upkeep::Replay(2, 3),
        ];
        for (asserted, retracted) in &history {
            naive_claims = naive(&naive_claims, asserted, retracted);
            let oracle = State::from_claims(naive_claims.clone());
            for (state, upkeep) in layered.iter_mut().zip(upkeeps) {
                match upkeep {
                    Upkeep::Threshold(threshold) => state.apply_under(asserted, retracted, threshold),
                    Upkeep::Replay(dead, overlay) => {
                        state.apply_replayed_under(asserted, retracted, dead, overlay);
                    }
                }
                prop_assert_eq!(state.claims().to_vec(), naive_claims.clone());
                prop_assert_eq!(&*state, &oracle);
                prop_assert_eq!(state.len(), naive_claims.len());
                for claim in &naive_claims {
                    prop_assert!(state.contains(claim));
                }
                for predicate in ["P", "Q", "R"] {
                    let per_predicate: Vec<ClaimInstance> = state.claims_for(predicate).cloned().collect();
                    let expected: Vec<ClaimInstance> = naive_claims
                        .iter()
                        .filter(|c| c.predicate.as_str() == predicate)
                        .cloned()
                        .collect();
                    prop_assert_eq!(per_predicate, expected);
                }
                prop_assert_eq!(buckets(state), expected_buckets(&naive_claims));
            }
        }
    }
}

/// Retracting and re-admitting a claim appends it at the tail, never
/// reviving its old position - in the base and in the overlay alike.
#[test]
fn a_readmitted_claim_moves_to_the_tail() {
    let c = |n: u8| ClaimInstance {
        predicate: "P".into(),
        args: vec![EvalValue::Decimal(rust_decimal::Decimal::from(n))],
    };
    let mut state = State::from_claims(vec![c(1), c(2), c(3)]);
    state.apply_under(&[c(2)], &[c(2)], usize::MAX);
    assert_eq!(state.claims().to_vec(), vec![c(1), c(3), c(2)]);
    // Now c(2) lives in the overlay: retract and readmit it again.
    state.apply_under(&[], &[c(2)], usize::MAX);
    assert_eq!(state.claims().to_vec(), vec![c(1), c(3)]);
    state.apply_under(&[c(4), c(2)], &[], usize::MAX);
    assert_eq!(state.claims().to_vec(), vec![c(1), c(3), c(4), c(2)]);
    assert_eq!(state.len(), 4);
}

/// What the evaluator reads of a candidate, through the view over the
/// pre-state and the staged delta, is what it reads of the owned
/// candidate `with_delta` builds: every predicate's claims in order, and
/// every argument bucket, retractions and re-admissions included.
fn view_reads(state: StateRef<'_>, domain: &[EvalValue]) -> Vec<(String, Vec<ClaimInstance>)> {
    let mut out = Vec::new();
    for predicate in ["P", "Q", "R"] {
        let name = PredicateName::from(predicate);
        out.push((
            predicate.to_string(),
            state.claims_for_name(&name).cloned().collect(),
        ));
        for pos in 0..2 {
            for value in domain {
                let bucket: Option<Vec<ClaimInstance>> = state
                    .claim_candidates(&name, pos, value)
                    .map(|b| b.iter().cloned().collect());
                out.push((
                    format!("{predicate}[{pos}]={value:?}"),
                    bucket.unwrap_or_default(),
                ));
            }
        }
    }
    out
}

proptest! {
    #[test]
    fn a_candidate_view_reads_as_the_owned_candidate(
        seed in proptest::collection::vec(claim_strategy(), 0..12),
        history in proptest::collection::vec(delta_strategy(), 1..6),
        (asserted, retracted) in delta_strategy(),
    ) {
        // A pre-state with an overlay and dead entries of its own.
        let mut pre = State::from_claims(naive(&[], &seed, &[]));
        for (a, r) in &history {
            pre.apply_under(a, r, 2);
        }
        // Every value the history ever carried, so a bucket a past step
        // created or emptied is asked for too.
        let domain: Vec<EvalValue> = seed
            .iter()
            .chain(history.iter().flat_map(|(a, r)| a.iter().chain(r)))
            .chain(&asserted)
            .chain(&retracted)
            .flat_map(|c| c.args.clone())
            .collect();
        let owned = pre.with_delta(&asserted, &retracted);
        let candidate = Candidate::new(&pre, &asserted, &retracted);
        prop_assert_eq!(
            view_reads(StateRef::Candidate(&candidate), &domain),
            view_reads(StateRef::Owned(&owned), &domain)
        );
        // The pre-state itself reads the same through either door.
        prop_assert_eq!(
            view_reads(StateRef::Owned(&pre), &domain),
            view_reads((&pre).into(), &domain)
        );
    }
}

/// A replay's upkeep compacts on each of its two causes exactly where
/// its floor is crossed, and a compaction moves nothing in the logical
/// state: the claims and their order are those of the ordinary upkeep
/// over the same changes, a re-admission at the tail included.
#[test]
fn a_replay_compacts_on_each_cause_and_keeps_the_logical_state() {
    let x = |i: usize| ClaimInstance {
        predicate: "X".into(),
        args: vec![EvalValue::Subject(format!("s{i}").into())],
    };
    let rebuilds = || crate::tally::snapshot()[2];
    let mut state = State::default();
    // Every change, so the ordinary upkeep can fold the same ones
    // afterwards, outside the counted stretches below.
    let mut changes: Vec<(Vec<ClaimInstance>, Vec<ClaimInstance>)> = Vec::new();
    let mut apply =
        |state: &mut State, asserted: Vec<ClaimInstance>, retracted: Vec<ClaimInstance>| {
            state.apply_replayed(&asserted, &retracted);
            changes.push((asserted, retracted));
        };

    // Live overlay growth: past 4,096 live overlay entries over an
    // empty base, and not before.
    let before = rebuilds();
    for i in 0..4096 {
        apply(&mut state, vec![x(i)], vec![]);
    }
    assert_eq!(rebuilds() - before, 0, "4,096 live entries do not compact");
    apply(&mut state, vec![x(4096)], vec![]);
    assert_eq!(rebuilds() - before, 1, "the 4,097th live entry compacts");
    assert_eq!(state.layering().overlay_live, 0);

    // Dead entries: more dead than live, here past 2,048 retractions
    // of the 4,097, and not before.
    let before = rebuilds();
    for i in 0..2048 {
        apply(&mut state, vec![], vec![x(i)]);
    }
    assert_eq!(rebuilds() - before, 0, "2,048 dead against 2,049 live");
    apply(&mut state, vec![], vec![x(2048)]);
    assert_eq!(rebuilds() - before, 1, "the 2,049th dead entry compacts");
    assert_eq!(state.layering().base_dead, 0);

    // A claim retracted and admitted again goes to the tail.
    apply(&mut state, vec![x(0)], vec![]);
    let mut expected: Vec<ClaimInstance> = (2049..4097).map(x).collect();
    expected.push(x(0));
    assert_eq!(state.claims().to_vec(), expected);

    let mut reference = State::default();
    let before = rebuilds();
    for (asserted, retracted) in &changes {
        reference.apply(asserted, retracted);
    }
    assert!(rebuilds() > before, "the ordinary upkeep compacted too");
    assert!(
        state == reference,
        "the same logical state as the ordinary upkeep"
    );
}
