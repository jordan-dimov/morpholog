//! Checking what the audit log says happened, one row at a time in the
//! canonical order, with nothing but the rows.
//!
//! The legacy prefix, written before the log had one order, is replayed
//! from its proposed lists and compared with the baseline the activation
//! record names. Overlapping legacy writers could commit something other
//! than what they proposed, so a disagreement makes that history
//! uncertain, not forged. Every later row records what its commit did:
//! each delete must find its claim and each insert must not. A no-op
//! takes no lock, so a delete of the same claim can lawfully come before
//! it, and it is never checked against the state.

use std::collections::{HashMap, HashSet};

use morpholog_core::ClaimInstance;
use serde::Serialize;
use uuid::Uuid;

use crate::activation::{ACTIVATION_MODEL_HASH, PARAMETERS, activation_arguments};
use crate::attestation::AuditAttestation;
use crate::audit::{AuditRow, Operation, OperationKind};
use crate::claims::ClaimRead;
use crate::merkle::{Frontier, audit_leaf_hash};
use crate::replay::EffectKind;

/// A row whose record contradicts the history before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplayConflict {
    pub transition_id: Uuid,
    pub detail: String,
}

#[derive(Default)]
pub(crate) struct HistoryCheck {
    /// The replayed claims, each with the transition that admitted it.
    admitted_by: HashMap<ClaimInstance, Uuid>,
    legacy: Frontier,
    activated: bool,
    rows: i64,
    legacy_disagreement: Option<LegacyDisagreement>,
    conflicts: Vec<ReplayConflict>,
}

/// Where the legacy replay and the activation baseline differ, each claim
/// with the transition that side says admitted it. A claim both hold but
/// attribute differently is in both lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyDisagreement {
    pub(crate) only_in_baseline: Vec<ClaimRead>,
    pub(crate) only_in_replay: Vec<ClaimRead>,
}

pub(crate) struct HistoryVerdict {
    pub(crate) rows: i64,
    pub(crate) admitted_by: HashMap<ClaimInstance, Uuid>,
    pub(crate) legacy_disagreement: Option<LegacyDisagreement>,
    pub(crate) conflicts: Vec<ReplayConflict>,
}

impl HistoryCheck {
    /// The next row in canonical order.
    pub(crate) fn push(&mut self, row: &AuditRow) -> Result<(), serde_json::Error> {
        self.rows += 1;
        let effect = row.effect();
        let mut faults: Vec<String> = Vec::new();
        let migration = matches!(row.attestation, Some(AuditAttestation::Migration { .. }));
        match effect.kind {
            EffectKind::Legacy { .. } => {
                self.legacy.push(audit_leaf_hash(row)?);
            }
            EffectKind::Activation { baseline } => {
                if self.activated {
                    faults.push("a second activation record".into());
                }
                faults.extend(activation_faults(row, &self.legacy)?);
                let replayed = std::mem::take(&mut self.admitted_by);
                let claims: HashMap<ClaimInstance, Uuid> = baseline
                    .iter()
                    .filter_map(|op| op.asserted_in.map(|by| (op.claim(), by)))
                    .collect();
                let only_in_baseline = unmatched(&claims, &replayed);
                let only_in_replay = unmatched(&replayed, &claims);
                if !only_in_baseline.is_empty() || !only_in_replay.is_empty() {
                    self.legacy_disagreement = Some(LegacyDisagreement {
                        only_in_baseline,
                        only_in_replay,
                    });
                }
                self.activated = true;
            }
            EffectKind::Ordered { operations } => {
                if migration {
                    faults.push(
                        "a migration attestation on a row that is not the activation record".into(),
                    );
                }
                if self.legacy.len() > 0 && !self.activated {
                    faults
                        .push("ordered after legacy rows with no activation record between".into());
                    self.activated = true;
                }
                if !follows_the_proposal(row, operations)? {
                    faults.push("the operations do not follow the claims it proposed".into());
                }
                // Presence as the row's own earlier operations left it.
                let mut moved: HashMap<ClaimInstance, bool> = HashMap::new();
                for op in operations {
                    let claim = op.claim();
                    let present = moved
                        .get(&claim)
                        .copied()
                        .unwrap_or_else(|| self.admitted_by.contains_key(&claim));
                    match op.op {
                        OperationKind::Del => moved.insert(claim.clone(), false),
                        OperationKind::Ins => moved.insert(claim.clone(), true),
                        OperationKind::Noop => None,
                    };
                    let fault = match op.op {
                        _ if op.asserted_in.is_some() => Some(
                            "names the transition that admitted it, which only the baseline may",
                        ),
                        OperationKind::Del if !present => Some("finds no such claim"),
                        OperationKind::Ins if present => Some("finds the claim already present"),
                        _ => None,
                    };
                    if let Some(fault) = fault {
                        faults.push(format!(
                            "{} {} {} {fault}",
                            op_name(op.op),
                            claim.predicate,
                            serde_json::to_string(&claim.args)?
                        ));
                    }
                }
            }
        }
        effect.apply_to_provenance(&mut self.admitted_by);
        self.conflicts
            .extend(faults.into_iter().map(|detail| ReplayConflict {
                transition_id: row.transition_id,
                detail,
            }));
        Ok(())
    }

    pub(crate) fn finish(self) -> HistoryVerdict {
        HistoryVerdict {
            rows: self.rows,
            admitted_by: self.admitted_by,
            legacy_disagreement: self.legacy_disagreement,
            conflicts: self.conflicts,
        }
    }
}

/// The claims of `side` that `other` does not hold with the same admitter.
fn unmatched(
    side: &HashMap<ClaimInstance, Uuid>,
    other: &HashMap<ClaimInstance, Uuid>,
) -> Vec<ClaimRead> {
    let mut reads: Vec<ClaimRead> = side
        .iter()
        .filter(|(claim, by)| other.get(*claim) != Some(*by))
        .map(|(claim, by)| ClaimRead {
            claim: claim.clone(),
            asserted_in: *by,
        })
        .collect();
    reads.sort_by_key(|r| sort_key(&r.claim));
    reads
}

/// The commit deletes each distinct claim it retracts, in order, then
/// inserts each claim it admits or finds it present, in order.
fn follows_the_proposal(
    row: &AuditRow,
    operations: &[Operation],
) -> Result<bool, serde_json::Error> {
    let mut seen = HashSet::new();
    let mut expected: Vec<(&ClaimInstance, bool)> = Vec::new();
    for claim in &row.retracted_claims {
        if seen.insert((claim.predicate.clone(), serde_json::to_string(&claim.args)?)) {
            expected.push((claim, true));
        }
    }
    expected.extend(row.asserted_claims.iter().map(|claim| (claim, false)));
    Ok(operations.len() == expected.len()
        && operations
            .iter()
            .zip(expected)
            .all(|(op, (claim, deletes))| {
                op.predicate == claim.predicate
                    && op.args == claim.args
                    && (op.op == OperationKind::Del) == deletes
            }))
}

/// What makes this row other than the record the migration writes over
/// exactly this legacy prefix.
fn activation_faults(row: &AuditRow, legacy: &Frontier) -> Result<Vec<String>, serde_json::Error> {
    let mut faults = Vec::new();
    if (row.audit_order, row.act_ordinal) != (Some(1), Some(0)) {
        faults.push("the activation record is not the first order".to_string());
    }
    if !matches!(row.attestation, Some(AuditAttestation::Migration { .. })) {
        faults.push("the activation record carries no migration attestation".to_string());
    }
    if row.parameters.as_deref() != Some(&PARAMETERS.map(String::from)[..]) {
        faults.push("the activation record's parameters are not its own".to_string());
    }
    if row.model_hash.as_deref() != Some(ACTIVATION_MODEL_HASH) {
        faults.push("the activation record names another model".to_string());
    }
    if !row.asserted_claims.is_empty()
        || !row.retracted_claims.is_empty()
        || !row.emitted_intents.is_empty()
    {
        faults.push("the activation record proposes claims or intents".to_string());
    }
    let baseline = row.operations.as_deref().unwrap_or_default();
    if baseline
        .iter()
        .any(|op| op.op != OperationKind::Ins || op.asserted_in.is_none())
    {
        faults.push(
            "the baseline holds an operation other than an insert naming its admitter".to_string(),
        );
    }
    if row.arguments != activation_arguments(legacy, baseline)? {
        faults.push(format!(
            "the activation record does not name the {} legacy rows before it and its own baseline",
            legacy.len()
        ));
    }
    Ok(faults)
}

fn op_name(op: OperationKind) -> &'static str {
    match op {
        OperationKind::Del => "del",
        OperationKind::Ins => "ins",
        OperationKind::Noop => "noop",
    }
}

pub(crate) fn sorted(claims: impl Iterator<Item = ClaimInstance>) -> Vec<ClaimInstance> {
    let mut claims: Vec<ClaimInstance> = claims.collect();
    claims.sort_by_key(sort_key);
    claims
}

pub(crate) fn sort_key(c: &ClaimInstance) -> (String, String) {
    (
        c.predicate.to_string(),
        serde_json::to_string(&c.args).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u128, name: &str) -> AuditRow {
        serde_json::from_value(serde_json::json!({
            "transition_id": Uuid::from_u128(id),
            "transformation_name": name,
            "arguments": [],
            "actor": { "type": "subject", "value": "w" },
            "invariant_epoch": 1,
            "invariants_checked": [],
            "asserted_claims": [],
            "retracted_claims": [],
            "emitted_intents": [],
            "committed_at": "2026-06-24T00:00:00Z",
        }))
        .unwrap()
    }

    fn x() -> ClaimInstance {
        ClaimInstance {
            predicate: "X".into(),
            args: Vec::new(),
        }
    }

    /// A legacy row admitting X, and the activation record over it.
    fn history() -> (AuditRow, AuditRow) {
        let legacy = AuditRow {
            asserted_claims: vec![x()],
            ..row(1, "admit_x")
        };
        let mut frontier = Frontier::default();
        frontier.push(audit_leaf_hash(&legacy).unwrap());
        let baseline = vec![Operation {
            asserted_in: Some(legacy.transition_id),
            ..Operation::new(OperationKind::Ins, &x())
        }];
        let activation = AuditRow {
            arguments: activation_arguments(&frontier, &baseline).unwrap(),
            attestation: Some(AuditAttestation::Migration {
                authenticated_by: "owner".into(),
                authenticated_by_oid: 10,
            }),
            parameters: Some(PARAMETERS.map(String::from).to_vec()),
            model_hash: Some(ACTIVATION_MODEL_HASH.into()),
            semantics_version: Some(1),
            drawn_subjects: Some(Vec::new()),
            audit_order: Some(1),
            act_ordinal: Some(0),
            ordered_at: Some(legacy.committed_at),
            operations: Some(baseline),
            ..row(2, crate::activation::ACTIVATE)
        };
        (legacy, activation)
    }

    fn conflicts(rows: &[AuditRow]) -> Vec<String> {
        let mut check = HistoryCheck::default();
        for row in rows {
            check.push(row).unwrap();
        }
        check
            .finish()
            .conflicts
            .into_iter()
            .map(|c| c.detail)
            .collect()
    }

    /// A pack is hostile input with no database constraints behind it, so
    /// each part of the record's identity is held on its own.
    #[test]
    fn each_rule_of_the_history_check_finds_its_own_fault() {
        let (legacy, activation) = history();
        assert_eq!(
            conflicts(&[legacy.clone(), activation.clone()]),
            Vec::<String>::new()
        );

        type Edit = fn(&mut AuditRow);
        let edits: Vec<(&str, Edit)> = vec![
            ("not the first order", |r| r.audit_order = Some(2)),
            ("no migration attestation", |r| {
                r.attestation = Some(AuditAttestation::Gateway {
                    authenticated_by: "owner".into(),
                    authenticated_by_oid: None,
                })
            }),
            ("parameters are not its own", |r| {
                r.parameters = Some(vec!["a".into(), "b".into(), "c".into()])
            }),
            ("another model", |r| {
                r.model_hash = Some(format!("sha256:{}", "e".repeat(64)))
            }),
            ("proposes claims", |r| r.asserted_claims = vec![x()]),
            ("other than an insert", |r| {
                r.operations.as_mut().unwrap()[0].asserted_in = None
            }),
            ("legacy rows before it", |r| r.arguments.reverse()),
        ];
        for (fault, edit) in edits {
            let mut forged = activation.clone();
            edit(&mut forged);
            let found = conflicts(&[legacy.clone(), forged]);
            // A changed baseline no longer matches its digest either.
            let alone = if fault == "other than an insert" {
                2
            } else {
                1
            };
            assert!(
                found.len() == alone && found[0].contains(fault),
                "{fault}: {found:?}"
            );
        }

        let mut second = activation.clone();
        second.transition_id = Uuid::from_u128(3);
        let found = conflicts(&[legacy.clone(), activation.clone(), second]);
        assert!(
            found
                .iter()
                .any(|f| f.contains("a second activation record")),
            "{found:?}"
        );

        let ordered = AuditRow {
            transition_id: Uuid::from_u128(4),
            transformation_name: "admit_y".into(),
            audit_order: Some(2),
            operations: Some(Vec::new()),
            ..activation.clone()
        };
        let found = conflicts(&[legacy, activation, ordered]);
        assert!(
            found.len() == 1 && found[0].contains("migration attestation on a row"),
            "{found:?}"
        );

        let naming = AuditRow {
            transition_id: Uuid::from_u128(5),
            transformation_name: "admit_y".into(),
            attestation: Some(AuditAttestation::Gateway {
                authenticated_by: "w".into(),
                authenticated_by_oid: None,
            }),
            audit_order: Some(2),
            retracted_claims: vec![x()],
            operations: Some(vec![Operation {
                asserted_in: Some(Uuid::from_u128(1)),
                ..Operation::new(OperationKind::Del, &x())
            }]),
            ..history().1
        };
        let found = conflicts(&[history().0, history().1, naming]);
        assert!(
            found.len() == 1 && found[0].contains("which only the baseline may"),
            "{found:?}"
        );
    }
}
