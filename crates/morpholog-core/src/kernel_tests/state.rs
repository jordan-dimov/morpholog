//! The layered state is invisible: every physical layering of one
//! logical history answers exactly as the naive rebuild does.

use proptest::prelude::*;

use super::*;

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

/// Today's algorithm, kept as the oracle: copy, drop every retracted
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

proptest! {
    #[test]
    fn every_layering_reads_as_the_naive_rebuild(
        seed in proptest::collection::vec(claim_strategy(), 0..12),
        history in proptest::collection::vec(delta_strategy(), 1..12),
    ) {
        let mut naive_claims = naive(&[], &seed, &[]);
        let base = State::from_claims(naive_claims.clone());
        // Compaction on every change, every few, and never: one logical
        // history, four physical ones.
        let mut layered = [base.clone(), base.clone(), base.clone(), base];
        let thresholds = [0usize, 2, 7, usize::MAX];
        for (asserted, retracted) in &history {
            naive_claims = naive(&naive_claims, asserted, retracted);
            let oracle = State::from_claims(naive_claims.clone());
            for (state, threshold) in layered.iter_mut().zip(thresholds) {
                state.apply_under(asserted, retracted, threshold);
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
