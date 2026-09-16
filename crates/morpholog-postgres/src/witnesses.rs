//! The witness axis of a verification: what each stored external witness
//! proves about its checkpoint, judged offline against the anchors the
//! verifier chose. Independent of the tree verdict - a witness says "this
//! head existed no later than T" whether or not the log still recomputes
//! to that head - and never part of it: only an `invalid` witness is a
//! judgement, and only that fails the command.

use chrono::{DateTime, Utc};
use morpholog_witness::{Anchors, WitnessStatus, verify_rfc3161};
use serde::Serialize;

use crate::checkpoints::{Checkpoint, WitnessScheme};
use crate::signing::{TreeHead, tree_head_witness_bytes};

pub use morpholog_witness::Anchors as WitnessAnchors;

/// One stored witness, judged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WitnessVerdict {
    pub scheme: WitnessScheme,
    pub submitted_to: String,
    pub status: WitnessStanding,
    #[serde(skip_serializing_if = "Option::is_none")]
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
                prev_checkpoint_hash: c.prev_checkpoint_hash.as_deref(),
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
            } => verdict(
                WitnessStanding::Untrusted,
                Some(attested_at),
                Some(format!(
                    "signer `{signer}` chains to none of the supplied anchors"
                )),
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
