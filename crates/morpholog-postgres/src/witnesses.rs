//! The witness axis of a verification: what each stored external witness
//! proves about its checkpoint, judged offline against the anchors the
//! verifier chose. Independent of the tree verdict - a witness says "this
//! head existed no later than T" whether or not the log still recomputes
//! to that head - and never part of it: only an `invalid` witness is a
//! judgement, and only that fails the command.

use chrono::{DateTime, Utc};
use morpholog_witness::{Anchors, WitnessStatus, verify_rfc3161};
use serde::Serialize;

use crate::checkpoints::TreeVerification;
use crate::checkpoints::{Checkpoint, WitnessScheme};
use crate::pack::{SelectiveVerification, WindowVerification};
use crate::signing::{TreeHead, tree_head_witness_bytes};

pub use morpholog_witness::Anchors as WitnessAnchors;

/// One stored witness, judged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WitnessVerdict {
    pub scheme: WitnessScheme,
    pub submitted_to: String,
    pub status: WitnessStanding,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(with = "crate::wire_time::option")]
    pub attested_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What the verifier could establish, with the trust material it had.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WitnessStanding {
    Verified,
    Untrusted,
    Unverified,
    Unsupported,
    Invalid,
}

/// A checkpoint's witnesses, judged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckpointWitnesses {
    pub tree_size: i64,
    pub witnesses: Vec<WitnessVerdict>,
}

/// The whole axis: every witnessed checkpoint in the order given, and the
/// earliest time any VERIFIED witness attests - the one figure a reader
/// can rest a "no later than" claim on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WitnessesReport {
    pub checkpoints: Vec<CheckpointWitnesses>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(with = "crate::wire_time::option")]
    pub earliest_attested_at: Option<DateTime<Utc>>,
}

impl WitnessesReport {
    /// Whether any witness was judged and found wrong.
    pub fn any_invalid(&self) -> bool {
        self.checkpoints
            .iter()
            .flat_map(|c| &c.witnesses)
            .any(|w| w.status == WitnessStanding::Invalid)
    }
}

/// Judge every witness on the given checkpoints. `None` when no checkpoint
/// carries one, so a report without witnesses stays byte-identical to
/// before the axis existed.
pub fn witnesses_report(
    checkpoints: &[Checkpoint],
    anchors: Option<&Anchors>,
) -> Option<WitnessesReport> {
    let judged: Vec<CheckpointWitnesses> = checkpoints
        .iter()
        .filter(|c| !c.witnesses.is_empty())
        .map(|c| {
            let head = TreeHead {
                tree_size: c.tree_size,
                root_hash: &c.root_hash,
                prev_checkpoint_hash: c.prev_checkpoint_hash.as_ref(),
                checkpoint_hash: &c.checkpoint_hash,
            };
            let payload = tree_head_witness_bytes(&head);
            CheckpointWitnesses {
                tree_size: c.tree_size,
                witnesses: c
                    .witnesses
                    .iter()
                    .map(|w| judge(w, &payload, anchors))
                    .collect(),
            }
        })
        .collect();
    if judged.is_empty() {
        return None;
    }
    let earliest_attested_at = judged
        .iter()
        .flat_map(|c| &c.witnesses)
        .filter(|w| w.status == WitnessStanding::Verified)
        .filter_map(|w| w.attested_at)
        .min();
    Some(WitnessesReport {
        checkpoints: judged,
        earliest_attested_at,
    })
}

fn judge(
    witness: &crate::checkpoints::Witness,
    payload: &[u8],
    anchors: Option<&Anchors>,
) -> WitnessVerdict {
    use base64::Engine as _;
    let verdict = |status, attested_at, detail| WitnessVerdict {
        scheme: witness.scheme,
        submitted_to: witness.submitted_to.clone(),
        status,
        attested_at,
        detail,
    };
    let proof = match base64::engine::general_purpose::STANDARD.decode(&witness.proof) {
        Ok(bytes) => bytes,
        Err(e) => {
            return verdict(
                WitnessStanding::Invalid,
                None,
                Some(format!("the stored proof is not base64: {e}")),
            );
        }
    };
    match witness.scheme {
        WitnessScheme::Rfc3161 => match verify_rfc3161(&proof, payload, anchors) {
            WitnessStatus::Verified { attested_at } => {
                verdict(WitnessStanding::Verified, Some(attested_at), None)
            }
            WitnessStatus::Untrusted {
                attested_at,
                signer,
                reason,
            } => verdict(
                WitnessStanding::Untrusted,
                Some(attested_at),
                Some(format!("signer `{signer}`: {reason}")),
            ),
            WitnessStatus::Unverified { attested_at } => verdict(
                WitnessStanding::Unverified,
                Some(attested_at),
                Some("no trust anchors were supplied".to_string()),
            ),
            WitnessStatus::Unsupported { detail } => {
                verdict(WitnessStanding::Unsupported, None, Some(detail))
            }
            WitnessStatus::Invalid { detail } => {
                verdict(WitnessStanding::Invalid, None, Some(detail))
            }
        },
    }
}

/// `audit verify-pack` with the witness axis requested: the pack's own
/// verdict, whatever its kind, beside what its checkpoints' witnesses
/// prove. Emitted only on request, so a verifier that never asked keeps
/// the bare verdict it always had.
#[derive(Debug, Clone, Serialize)]
pub struct PackVerificationReport {
    pub verdict: PackVerdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub witnesses: Option<WitnessesReport>,
}

/// One of the three pack verdicts, serialised as itself.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum PackVerdict {
    Prefix(TreeVerification),
    Window(WindowVerification),
    Selective(SelectiveVerification),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoints::Witness;
    use base64::Engine as _;

    const FIXTURES: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../morpholog-witness/tests/fixtures/rfc3161/"
    );

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{FIXTURES}{name}")).unwrap()
    }

    /// The head the recorded DigiCert token was obtained over: the same
    /// sample head the frozen witness payload test pins.
    fn witnessed_head() -> Checkpoint {
        Checkpoint {
            tree_size: 42,
            root_hash: format!("sha256:{}", "1".repeat(64)).parse().unwrap(),
            prev_checkpoint_hash: None,
            checkpoint_hash: format!("sha256:{}", "2".repeat(64)).parse().unwrap(),
            signatures: Vec::new(),
            witnesses: vec![Witness {
                scheme: WitnessScheme::Rfc3161,
                proof: base64::engine::general_purpose::STANDARD
                    .encode(fixture("genesis_digicert.tsr")),
                submitted_to: "http://timestamp.digicert.com".into(),
            }],
        }
    }

    fn anchors(name: &str) -> Anchors {
        Anchors::from_pem(&fixture(name)).unwrap()
    }

    #[test]
    fn a_recorded_token_is_judged_by_what_the_verifier_trusts() {
        let chain = vec![witnessed_head()];

        let report = witnesses_report(&chain, Some(&anchors("digicert_chain.pem"))).unwrap();
        let [cp] = report.checkpoints.as_slice() else {
            panic!("one witnessed checkpoint")
        };
        assert_eq!(cp.tree_size, 42);
        assert_eq!(cp.witnesses[0].status, WitnessStanding::Verified);
        assert!(cp.witnesses[0].attested_at.is_some());
        assert_eq!(report.earliest_attested_at, cp.witnesses[0].attested_at);
        assert!(!report.any_invalid());

        let report = witnesses_report(&chain, Some(&anchors("unrelated.pem"))).unwrap();
        assert_eq!(
            report.checkpoints[0].witnesses[0].status,
            WitnessStanding::Untrusted
        );
        assert!(report.checkpoints[0].witnesses[0].attested_at.is_some());
        assert_eq!(
            report.earliest_attested_at, None,
            "only a VERIFIED time counts"
        );

        let report = witnesses_report(&chain, None).unwrap();
        assert_eq!(
            report.checkpoints[0].witnesses[0].status,
            WitnessStanding::Unverified
        );
        assert_eq!(report.earliest_attested_at, None);
    }

    #[test]
    fn a_token_moved_to_another_head_is_invalid_and_the_only_failing_standing() {
        // Attacker capability: rewrites the checkpoint a genuine token is
        // attached to, hoping the timestamp vouches for the new head.
        let mut moved = witnessed_head();
        moved.tree_size = 43;
        let report = witnesses_report(&[moved], Some(&anchors("digicert_chain.pem"))).unwrap();
        let verdict = &report.checkpoints[0].witnesses[0];
        assert_eq!(verdict.status, WitnessStanding::Invalid);
        assert_eq!(verdict.attested_at, None);
        assert!(report.any_invalid());

        let mut garbled = witnessed_head();
        garbled.witnesses[0].proof = "not base64!".into();
        let report = witnesses_report(&[garbled], None).unwrap();
        assert_eq!(
            report.checkpoints[0].witnesses[0].status,
            WitnessStanding::Invalid
        );
    }

    #[test]
    fn a_chain_without_witnesses_has_no_axis() {
        let mut bare = witnessed_head();
        bare.witnesses.clear();
        assert!(witnesses_report(&[bare], None).is_none());
        assert!(witnesses_report(&[], None).is_none());
    }
}
