//! The keys-as-claims layer: which signing keys the ledger had authorised
//! at a point in its own history.
//!
//! A signing key is authorised by an admitted `AuditSigningKey(key_id,
//! purpose, public_key)` claim - an ordinary governed claim, admitted and
//! retracted through the operator's own transformations under their own
//! authority gate; the runtime does not own the authorisation rules, the
//! operator does. The verifier recognises only this exact shape - the name
//! `AuditSigningKey` with three `Subject` arguments in that order - so a
//! differently-shaped declaration of the same name authorises nothing.
//! That is not silent in practice: `checkpoint --signing-key` refuses to
//! sign with a key this fold does not find authorised, so a misshapen
//! declaration fails loudly at signing time, not quietly at verify.
//!
//! Authorisation is judged **as of the checkpoint's prefix**: a key valid
//! when a checkpoint was signed stays valid for that checkpoint even after
//! it is later revoked. Revocation stops *future* signing; it does not
//! rewrite past evidence - the same doctrine as a decision that keeps its
//! standing after the authority behind it is rescinded.
//!
//! Pure and synchronous: a fold over audit rows, no I/O. The live verifier
//! folds rows read from the database; the offline pack verifier folds the
//! pack's own rows - one implementation, so they cannot drift.

use std::collections::HashSet;

use morpholog_core::{ClaimInstance, EvalValue};

use crate::audit::AuditRow;

/// The predicate naming an authorised signing key. The operator declares
/// it and admits/retracts it; the verifier recognises it by name and exact
/// `(key_id, purpose, public_key): Subject` shape.
pub(crate) const AUDIT_SIGNING_KEY_PREDICATE: &str = "AuditSigningKey";

/// The exact authorisation a checkpoint signature must match:
/// `(key_id, purpose, public_key)`.
pub type KeyTriple = (String, String, String);

/// Read an `AuditSigningKey(key_id, purpose, public_key)` claim as its
/// triple, or `None` for any other claim or a wrong-shaped one.
fn key_triple(claim: &ClaimInstance) -> Option<KeyTriple> {
    if claim.predicate.as_str() != AUDIT_SIGNING_KEY_PREDICATE {
        return None;
    }
    match claim.args.as_slice() {
        [
            EvalValue::Subject(key_id),
            EvalValue::Subject(purpose),
            EvalValue::Subject(public_key),
        ] => Some((
            key_id.as_str().to_string(),
            purpose.as_str().to_string(),
            public_key.as_str().to_string(),
        )),
        _ => None,
    }
}

/// The authorised key triples in force as of the first `tree_size` audit
/// rows, given the rows in canonical `(committed_at, transition_id)` order.
/// Each row's retractions are applied before its assertions, matching the
/// kernel's candidate-build order.
pub(crate) fn authorized_keys_as_of(rows: &[AuditRow], tree_size: i64) -> HashSet<KeyTriple> {
    let mut keys = HashSet::new();
    let n = (tree_size.max(0) as usize).min(rows.len());
    for row in &rows[..n] {
        for claim in &row.retracted_claims {
            if let Some(triple) = key_triple(claim) {
                keys.remove(&triple);
            }
        }
        for claim in &row.asserted_claims {
            if let Some(triple) = key_triple(claim) {
                keys.insert(triple);
            }
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpholog_core::{PredicateName, Subject};

    fn key_claim(key_id: &str, purpose: &str, public_key: &str) -> ClaimInstance {
        ClaimInstance {
            predicate: PredicateName::from(AUDIT_SIGNING_KEY_PREDICATE),
            args: vec![
                EvalValue::Subject(Subject::from(key_id)),
                EvalValue::Subject(Subject::from(purpose)),
                EvalValue::Subject(Subject::from(public_key)),
            ],
        }
    }

    fn other_claim() -> ClaimInstance {
        ClaimInstance {
            predicate: PredicateName::from("Revenue"),
            args: vec![EvalValue::Subject(Subject::from("asset"))],
        }
    }

    fn row(asserted: Vec<ClaimInstance>, retracted: Vec<ClaimInstance>) -> AuditRow {
        AuditRow {
            transition_id: uuid::Uuid::nil(),
            transformation_name: "t".into(),
            arguments: vec![],
            actor: Subject::from("officer"),
            invariant_epoch: 1,
            invariants_checked: vec![],
            asserted_claims: asserted,
            retracted_claims: retracted,
            emitted_intents: vec![],
            committed_at: chrono::Utc::now(),
            attestation: None,
        }
    }

    fn triple(key_id: &str, purpose: &str, public_key: &str) -> KeyTriple {
        (key_id.into(), purpose.into(), public_key.into())
    }

    #[test]
    fn an_authorized_key_is_in_force_from_its_admission() {
        let rows = vec![
            row(vec![other_claim()], vec![]),
            row(vec![key_claim("k1", "audit_checkpoint_v1", "pub1")], vec![]),
        ];
        // As of row 1 (before the admission) the key is not yet authorised.
        assert!(authorized_keys_as_of(&rows, 1).is_empty());
        // As of row 2 it is.
        let at_2 = authorized_keys_as_of(&rows, 2);
        assert!(at_2.contains(&triple("k1", "audit_checkpoint_v1", "pub1")));
    }

    #[test]
    fn revocation_is_not_retroactive() {
        let rows = vec![
            row(vec![key_claim("k1", "audit_checkpoint_v1", "pub1")], vec![]),
            row(vec![other_claim()], vec![]),
            row(vec![], vec![key_claim("k1", "audit_checkpoint_v1", "pub1")]),
        ];
        // As of the prefix where it was still admitted (size 2), it is authorised.
        assert!(authorized_keys_as_of(&rows, 2).contains(&triple(
            "k1",
            "audit_checkpoint_v1",
            "pub1"
        )));
        // As of the full log (after the retraction), it is gone.
        assert!(authorized_keys_as_of(&rows, 3).is_empty());
    }

    #[test]
    fn the_exact_triple_matters() {
        let rows = vec![row(
            vec![key_claim("k1", "audit_checkpoint_v1", "pub1")],
            vec![],
        )];
        let keys = authorized_keys_as_of(&rows, 1);
        assert!(!keys.contains(&triple("k1", "audit_checkpoint_v1", "pub2")));
        assert!(!keys.contains(&triple("k1", "other_purpose", "pub1")));
        assert!(!keys.contains(&triple("k2", "audit_checkpoint_v1", "pub1")));
    }

    #[test]
    fn tree_size_beyond_the_rows_is_clamped() {
        let rows = vec![row(vec![key_claim("k1", "p", "pub1")], vec![])];
        assert_eq!(authorized_keys_as_of(&rows, 99).len(), 1);
        assert!(authorized_keys_as_of(&rows, 0).is_empty());
    }
}
