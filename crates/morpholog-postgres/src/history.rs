//! Checking what the audit log says happened, one row at a time in the
//! canonical order, with nothing but the rows.
//!
//! The legacy prefix, written before the log had one order, is replayed
//! from its proposed lists and compared with the baseline the activation
//! writes. Overlapping legacy writers could commit something other
//! than what they proposed, so a disagreement makes that history
//! uncertain, not forged. The activation is order 1 alone: baseline
//! chunks at ordinals 0 to k-1, each a row of inserts naming their
//! admitters, then the record at k naming the legacy prefix, k and a
//! digest over the chunks' leaf hashes. Every later row records what its
//! commit did: each delete must find its claim and each insert must not.
//! A no-op takes no lock, so a delete of the same claim can lawfully come
//! before it, and it is never checked against the state.

use std::collections::{HashMap, HashSet};

use crate::claim_key::arguments_hash;

use jiff::Timestamp;
use morpholog_core::{ClaimInstance, PredicateName};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::activation::{ACTIVATION_MODEL_HASH, PARAMETERS, arguments_over};
use crate::attestation::AuditAttestation;
use crate::audit::{AuditRow, Operation, OperationKind};
use crate::claims::ClaimRead;
use crate::merkle::{Digest, Frontier, Hash, audit_leaf_hash};
use crate::replay::EffectKind;

/// A row whose record contradicts the history before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplayConflict {
    pub transition_id: Uuid,
    pub detail: String,
}

/// The activation's rows as every verifier holds them, live and offline:
/// their shape, their places, the baseline's order by the claims table's
/// own key `(predicate_name, arguments_hash)`, strictly increasing across
/// every chunk, and the record's commitment to the legacy prefix and the
/// chunks. Structure only, in state that does not grow with the baseline;
/// whether the baseline is the right one is the live check's question.
pub(crate) struct ActivationFold {
    legacy_len: u64,
    legacy_root: Hash,
    chunks: u64,
    claims: u64,
    digest: Sha256,
    last_key: Option<(PredicateName, Hash)>,
    last_row: Uuid,
    complete: bool,
}

/// The instants along the log, as every verifier holds them: a row's
/// instant is its `ordered_at`, or `committed_at` before the order (see
/// [`AuditRow::instant`]). One order has one instant; distinct orders'
/// instants strictly increase, the first at or after the last legacy
/// one; legacy rows sort by their instant, so theirs never decrease by
/// construction. So an instant ends a prefix of the log that splits no
/// transaction, which is what an as-of timestamp resolves by.
#[derive(Default)]
pub(crate) struct Chronology {
    last: Option<(u64, Timestamp)>,
}

impl Chronology {
    /// The next row in log order; how its instant breaks the chronology,
    /// if it does.
    pub(crate) fn observe(&mut self, row: &AuditRow) -> Option<String> {
        let order = row.audit_order.unwrap_or(0);
        let instant = row.instant();
        let fault = self.last.and_then(|(prev_order, prev)| {
            if order == 0 {
                None
            } else if order == prev_order {
                (instant != prev)
                    .then(|| format!("ordered_at {instant} differs from its order's {prev}"))
            } else if prev_order == 0 {
                (instant < prev).then(|| {
                    format!(
                        "ordered_at {instant} is before the last legacy row's committed_at {prev}"
                    )
                })
            } else {
                (instant <= prev).then(|| {
                    format!("ordered_at {instant} is not after order {prev_order}'s {prev}")
                })
            }
        });
        self.last = Some((order, instant));
        fault
    }
}

/// Why an ordinary ordered row is out of place: inside the activation's
/// order, or after legacy rows that no activation ended. A database
/// `init` creates has neither legacy rows nor an activation, and its
/// ordinary rows begin at order 1.
pub(crate) fn ordered_row_misplaced(
    row: &AuditRow,
    legacy_rows: u64,
    activation: Option<&ActivationFold>,
) -> Option<&'static str> {
    if row.audit_order == Some(1) && activation.is_some() {
        Some("an ordinary row inside the activation's order")
    } else if legacy_rows > 0 && activation.is_none() {
        Some("ordered after legacy rows with no activation record between")
    } else {
        None
    }
}

impl ActivationFold {
    /// At the activation's first row, over the legacy rows before it.
    pub(crate) fn begin(legacy: &Frontier) -> Self {
        ActivationFold {
            legacy_len: legacy.len() as u64,
            legacy_root: legacy.root(),
            chunks: 0,
            claims: 0,
            digest: Sha256::new(),
            last_key: None,
            last_row: Uuid::nil(),
            complete: false,
        }
    }

    /// The next control row, with its leaf hash. The faults found; the
    /// fold moves on past each so a later one is still found.
    pub(crate) fn push(
        &mut self,
        row: &AuditRow,
        leaf: Hash,
    ) -> Result<Vec<String>, serde_json::Error> {
        self.last_row = row.transition_id;
        let mut faults = Vec::new();
        if row.audit_order != Some(1) {
            faults.push("the activation is not the first order".to_string());
        }
        if !matches!(row.attestation, Some(AuditAttestation::Migration { .. })) {
            faults.push("the activation row carries no migration attestation".to_string());
        }
        if row.model_hash.as_deref() != Some(ACTIVATION_MODEL_HASH) {
            faults.push("the activation row names another model".to_string());
        }
        if !row.asserted_claims.is_empty()
            || !row.retracted_claims.is_empty()
            || !row.emitted_intents.is_empty()
        {
            faults.push("the activation row proposes claims or intents".to_string());
        }
        match row.effect().kind {
            EffectKind::Baseline { operations, .. } => {
                self.chunk(row, operations, leaf, &mut faults)?;
            }
            EffectKind::Activation { .. } => self.record(row, &mut faults),
            _ => faults.push("not a row of the activation".to_string()),
        }
        Ok(faults)
    }

    fn chunk(
        &mut self,
        row: &AuditRow,
        operations: &[Operation],
        leaf: Hash,
        faults: &mut Vec<String>,
    ) -> Result<(), serde_json::Error> {
        if self.complete {
            faults.push("a baseline chunk after the activation record".to_string());
        }
        if row.act_ordinal != Some(self.chunks as u32) {
            faults.push(format!(
                "a baseline chunk at ordinal {} where chunk {} was expected",
                row.act_ordinal.unwrap_or(0),
                self.chunks
            ));
        }
        if !row.arguments.is_empty() || row.parameters.as_deref() != Some(&[]) {
            faults.push("a baseline chunk carries arguments".to_string());
        }
        if operations
            .iter()
            .any(|op| op.op != OperationKind::Ins || op.asserted_in.is_none())
        {
            faults.push(
                "the baseline holds an operation other than an insert naming its admitter"
                    .to_string(),
            );
        }
        for op in operations {
            let key = match arguments_hash(&op.args) {
                Ok(hash) => (op.predicate.clone(), hash),
                Err(why) => {
                    faults.push(format!(
                        "the baseline claim {} {} has no key: {why}",
                        op.predicate,
                        serde_json::to_string(&op.args)?
                    ));
                    continue;
                }
            };
            if let Some(last) = &self.last_key {
                match key.cmp(last) {
                    std::cmp::Ordering::Greater => {}
                    std::cmp::Ordering::Equal => faults.push(format!(
                        "the baseline holds {} {} twice",
                        op.predicate,
                        serde_json::to_string(&op.args)?
                    )),
                    std::cmp::Ordering::Less => faults.push(format!(
                        "baseline claims out of order: {} {} after {} {}",
                        op.predicate,
                        serde_json::to_string(&op.args)?,
                        last.0,
                        hex::encode(last.1)
                    )),
                }
            }
            self.last_key = Some(key);
        }
        self.digest.update(leaf);
        self.chunks += 1;
        self.claims += operations.len() as u64;
        Ok(())
    }

    fn record(&mut self, row: &AuditRow, faults: &mut Vec<String>) {
        if self.complete {
            faults.push("a second activation record".to_string());
        }
        if row.act_ordinal != Some(self.chunks as u32) {
            faults.push(format!(
                "the activation record at ordinal {} after {} baseline chunks",
                row.act_ordinal.unwrap_or(0),
                self.chunks
            ));
        }
        if row.parameters.as_deref() != Some(&PARAMETERS.map(String::from)[..]) {
            faults.push("the activation record's parameters are not its own".to_string());
        }
        if row.operations.as_deref().is_some_and(|ops| !ops.is_empty()) {
            faults.push("the activation record carries operations".to_string());
        }
        let digest = Digest::from_bytes(self.digest.clone().finalize().into());
        if row.arguments != arguments_over(self.legacy_len, self.legacy_root, self.chunks, &digest)
        {
            faults.push(format!(
                "the activation record does not name the {} legacy rows before it and its {} \
                 baseline chunks",
                self.legacy_len, self.chunks
            ));
        }
        self.complete = true;
    }

    /// The activation began and its record never came, with the row the
    /// fault is laid at.
    pub(crate) fn incomplete(&self) -> Option<(Uuid, String)> {
        (!self.complete).then(|| {
            (
                self.last_row,
                format!(
                    "the activation has {} baseline chunks and no record",
                    self.chunks
                ),
            )
        })
    }
}

#[derive(Default)]
pub(crate) struct HistoryCheck {
    /// The replayed claims, each with the transition that admitted it.
    admitted_by: HashMap<ClaimInstance, Uuid>,
    legacy: Frontier,
    /// The activation once its first row is met, and the legacy replay
    /// as it stood then.
    activation: Option<(ActivationFold, HashMap<ClaimInstance, Uuid>)>,
    /// Ordered rows have begun: after the record, or without one.
    activated: bool,
    /// The missing activation was reported once.
    reported_missing: bool,
    rows: i64,
    chronology: Chronology,
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
            EffectKind::Baseline { .. } | EffectKind::Activation { .. } => {
                let leaf = audit_leaf_hash(row)?;
                let (fold, replayed) = self.activation.get_or_insert_with(|| {
                    (
                        ActivationFold::begin(&self.legacy),
                        std::mem::take(&mut self.admitted_by),
                    )
                });
                faults.extend(fold.push(row, leaf)?);
                if matches!(effect.kind, EffectKind::Activation { .. }) {
                    // The chunks before the record left the baseline, each
                    // claim with its own admitter, in `admitted_by`.
                    let only_in_baseline = unmatched(&self.admitted_by, replayed);
                    let only_in_replay = unmatched(replayed, &self.admitted_by);
                    if !only_in_baseline.is_empty() || !only_in_replay.is_empty() {
                        self.legacy_disagreement = Some(LegacyDisagreement {
                            only_in_baseline,
                            only_in_replay,
                        });
                    }
                    self.activated = true;
                }
            }
            EffectKind::Ordered { operations } => {
                if migration {
                    faults.push(
                        "a migration attestation on a row that is not the activation record".into(),
                    );
                }
                if let Some(why) = ordered_row_misplaced(
                    row,
                    self.legacy.len() as u64,
                    self.activation.as_ref().map(|(fold, _)| fold),
                ) && !self.reported_missing
                {
                    faults.push(why.into());
                    self.reported_missing = self.activation.is_none();
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
        faults.extend(self.chronology.observe(row));
        effect.apply_to_provenance(&mut self.admitted_by);
        self.conflicts
            .extend(faults.into_iter().map(|detail| ReplayConflict {
                transition_id: row.transition_id,
                detail,
            }));
        Ok(())
    }

    pub(crate) fn finish(mut self) -> HistoryVerdict {
        if let Some((fold, _)) = &self.activation
            && let Some((transition_id, detail)) = fold.incomplete()
        {
            self.conflicts.push(ReplayConflict {
                transition_id,
                detail,
            });
        }
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
    use crate::activation::{ACTIVATE, BASELINE, baseline_digest};
    use morpholog_core::EvalValue;

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

    fn claim(predicate: &str) -> ClaimInstance {
        ClaimInstance {
            predicate: predicate.into(),
            args: Vec::new(),
        }
    }

    fn ins(predicate: &str, by: u128) -> Operation {
        Operation {
            asserted_in: Some(Uuid::from_u128(by)),
            ..Operation::new(OperationKind::Ins, &claim(predicate))
        }
    }

    /// The fields every row of the activation shares.
    fn control(id: u128, name: &str, ordinal: u32, committed_at: jiff::Timestamp) -> AuditRow {
        AuditRow {
            attestation: Some(AuditAttestation::Migration {
                authenticated_by: "owner".into(),
                authenticated_by_oid: 10,
            }),
            parameters: Some(Vec::new()),
            model_hash: Some(ACTIVATION_MODEL_HASH.into()),
            semantics_version: Some(1),
            drawn_subjects: Some(Vec::new()),
            audit_order: Some(1),
            act_ordinal: Some(ordinal),
            ordered_at: Some(committed_at),
            operations: Some(Vec::new()),
            ..row(id, name)
        }
    }

    /// A legacy row admitting X, one baseline chunk holding it, and the
    /// activation record over both.
    fn history() -> (AuditRow, AuditRow, AuditRow) {
        let legacy = AuditRow {
            asserted_claims: vec![claim("X")],
            ..row(1, "admit_x")
        };
        let chunk = AuditRow {
            operations: Some(vec![ins("X", 1)]),
            ..control(2, BASELINE, 0, legacy.committed_at)
        };
        (
            legacy.clone(),
            chunk.clone(),
            record(&[legacy], &[chunk], 3),
        )
    }

    fn record(legacy: &[AuditRow], chunks: &[AuditRow], id: u128) -> AuditRow {
        let mut frontier = Frontier::default();
        for row in legacy {
            frontier.push(audit_leaf_hash(row).unwrap());
        }
        let leaves: Vec<Hash> = chunks.iter().map(|c| audit_leaf_hash(c).unwrap()).collect();
        AuditRow {
            arguments: arguments_over(
                frontier.len() as u64,
                frontier.root(),
                leaves.len() as u64,
                &baseline_digest(&leaves),
            ),
            parameters: Some(PARAMETERS.map(String::from).to_vec()),
            ..control(id, ACTIVATE, chunks.len() as u32, row(1, "").committed_at)
        }
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

    fn assert_names(found: &[String], fault: &str) {
        assert!(
            found.iter().any(|f| f.contains(fault)),
            "{fault}: {found:?}"
        );
    }

    /// A pack is hostile input with no database constraints behind it, so
    /// each part of the activation's shape is held on its own.
    #[test]
    fn each_rule_of_the_history_check_finds_its_own_fault() {
        let (legacy, chunk, activation) = history();
        assert_eq!(
            conflicts(&[legacy.clone(), chunk.clone(), activation.clone()]),
            Vec::<String>::new()
        );

        type Edit = fn(&mut AuditRow);
        let on_record: Vec<(&str, Edit)> = vec![
            ("not the first order", |r| {
                r.audit_order = Some(2);
                // A later order with a later instant, so only the place is wrong.
                r.ordered_at = r.ordered_at.map(|t| t + jiff::SignedDuration::from_secs(1));
            }),
            ("no migration attestation", |r| {
                r.attestation = Some(AuditAttestation::Gateway {
                    authenticated_by: "owner".into(),
                    authenticated_by_oid: None,
                })
            }),
            ("parameters are not its own", |r| {
                r.parameters = Some(vec!["a".into(), "b".into(), "c".into(), "d".into()])
            }),
            ("another model", |r| {
                r.model_hash = Some(format!("sha256:{}", "e".repeat(64)))
            }),
            ("proposes claims", |r| r.asserted_claims = vec![claim("X")]),
            ("carries operations", |r| {
                r.operations = Some(vec![ins("X", 1)])
            }),
            ("does not name the 1 legacy rows", |r| r.arguments.reverse()),
            ("at ordinal 0 after 1 baseline chunks", |r| {
                r.act_ordinal = Some(0)
            }),
        ];
        for (fault, edit) in on_record {
            let mut forged = activation.clone();
            edit(&mut forged);
            let found = conflicts(&[legacy.clone(), chunk.clone(), forged]);
            assert_eq!(found.len(), 1, "{fault}: {found:?}");
            assert_names(&found, fault);
        }

        // A changed chunk no longer matches the record's digest either.
        let on_chunk: Vec<(&str, Edit)> = vec![
            ("other than an insert", |r| {
                r.operations.as_mut().unwrap()[0].asserted_in = None
            }),
            ("at ordinal 1 where chunk 0 was expected", |r| {
                r.act_ordinal = Some(1)
            }),
            ("carries arguments", |r| {
                r.arguments = vec![EvalValue::Decimal(1.into())];
                r.parameters = Some(vec!["n".into()]);
            }),
            ("holds X [] twice", |r| {
                r.operations.as_mut().unwrap().push(ins("X", 1))
            }),
            ("out of order: A [] after X", |r| {
                r.operations.as_mut().unwrap().push(ins("A", 1))
            }),
        ];
        for (fault, edit) in on_chunk {
            let mut forged = chunk.clone();
            edit(&mut forged);
            let found = conflicts(&[legacy.clone(), forged, activation.clone()]);
            assert_eq!(found.len(), 2, "{fault}: {found:?}");
            assert_names(&found, fault);
            assert_names(&found, "does not name");
        }

        let mut second = activation.clone();
        second.transition_id = Uuid::from_u128(4);
        let found = conflicts(&[legacy.clone(), chunk.clone(), activation.clone(), second]);
        assert_names(&found, "a second activation record");

        let mut late = chunk.clone();
        late.transition_id = Uuid::from_u128(5);
        let found = conflicts(&[legacy.clone(), chunk.clone(), activation.clone(), late]);
        assert_names(&found, "a baseline chunk after the activation record");

        let found = conflicts(&[legacy.clone(), chunk.clone()]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_names(&found, "1 baseline chunks and no record");

        // A later order has a later instant.
        let later = activation
            .ordered_at
            .map(|t| t + jiff::SignedDuration::from_secs(1));
        let ordered = AuditRow {
            transition_id: Uuid::from_u128(6),
            transformation_name: "admit_y".into(),
            audit_order: Some(2),
            act_ordinal: Some(0),
            ordered_at: later,
            operations: Some(Vec::new()),
            ..activation.clone()
        };
        let found = conflicts(&[legacy.clone(), chunk.clone(), activation.clone(), ordered]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_names(&found, "migration attestation on a row");

        let naming = AuditRow {
            transition_id: Uuid::from_u128(7),
            transformation_name: "admit_y".into(),
            attestation: Some(AuditAttestation::Gateway {
                authenticated_by: "w".into(),
                authenticated_by_oid: None,
            }),
            arguments: Vec::new(),
            parameters: Some(Vec::new()),
            audit_order: Some(2),
            act_ordinal: Some(0),
            ordered_at: later,
            retracted_claims: vec![claim("X")],
            operations: Some(vec![Operation {
                asserted_in: Some(Uuid::from_u128(1)),
                ..Operation::new(OperationKind::Del, &claim("X"))
            }]),
            ..activation.clone()
        };
        let found = conflicts(&[legacy, chunk, activation, naming]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_names(&found, "which only the baseline may");
    }

    /// Two claims of one predicate in the key order the claims table
    /// keeps, `(predicate_name, arguments_hash)`, lower first.
    fn ordered_pair() -> (Operation, Operation) {
        let mut ops: Vec<Operation> = ["a", "b"]
            .into_iter()
            .map(|k| Operation {
                asserted_in: Some(Uuid::from_u128(1)),
                ..Operation::new(
                    OperationKind::Ins,
                    &ClaimInstance {
                        predicate: "X".into(),
                        args: vec![EvalValue::Subject(k.into())],
                    },
                )
            })
            .collect();
        ops.sort_by_key(|op| arguments_hash(&op.args).unwrap());
        let hi = ops.pop().unwrap();
        (ops.pop().unwrap(), hi)
    }

    /// The baseline's order is the table's whole key, so two claims of one
    /// predicate reversed are out of order, within a chunk and across two.
    #[test]
    fn same_predicate_claims_reversed_are_out_of_order() {
        let legacy = AuditRow {
            asserted_claims: vec![claim("X")],
            ..row(1, "admit_x")
        };
        let (lo, hi) = ordered_pair();
        let one = |ops: Vec<Operation>, ordinal: u32| AuditRow {
            operations: Some(ops),
            ..control(
                2 + u128::from(ordinal),
                BASELINE,
                ordinal,
                legacy.committed_at,
            )
        };
        let in_order = [one(vec![lo.clone(), hi.clone()], 0)];
        let rec = record(std::slice::from_ref(&legacy), &in_order, 9);
        assert!(conflicts(&[legacy.clone(), in_order[0].clone(), rec]).is_empty());

        let within = [one(vec![hi.clone(), lo.clone()], 0)];
        let rec = record(std::slice::from_ref(&legacy), &within, 9);
        let found = conflicts(&[legacy.clone(), within[0].clone(), rec]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_names(&found, "out of order");

        let across = [one(vec![hi], 0), one(vec![lo], 1)];
        let rec = record(std::slice::from_ref(&legacy), &across, 9);
        let found = conflicts(&[legacy, across[0].clone(), across[1].clone(), rec]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_names(&found, "out of order");
    }

    /// An ordinary row inside the activation's order is out of place.
    #[test]
    fn an_ordinary_row_inside_the_activation_is_a_fault() {
        let (legacy, chunk, activation) = history();
        let intruder = AuditRow {
            transition_id: Uuid::from_u128(8),
            transformation_name: "admit_y".into(),
            attestation: Some(AuditAttestation::Gateway {
                authenticated_by: "w".into(),
                authenticated_by_oid: None,
            }),
            arguments: Vec::new(),
            parameters: Some(Vec::new()),
            act_ordinal: Some(2),
            ..activation.clone()
        };
        let found = conflicts(&[legacy, chunk, activation, intruder]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_names(&found, "inside the activation's order");
    }

    /// The baseline keeps each claim's own admitter; a chunk's id never
    /// stands in for it, across chunks as within one.
    #[test]
    fn the_baseline_carries_its_own_provenance_across_chunks() {
        let legacy_x = AuditRow {
            asserted_claims: vec![claim("X")],
            ..row(1, "admit_x")
        };
        let legacy_y = AuditRow {
            asserted_claims: vec![claim("Y")],
            ..row(2, "admit_y")
        };
        let chunks = [
            AuditRow {
                operations: Some(vec![ins("X", 1)]),
                ..control(3, BASELINE, 0, legacy_x.committed_at)
            },
            AuditRow {
                operations: Some(vec![ins("Y", 2)]),
                ..control(4, BASELINE, 1, legacy_x.committed_at)
            },
        ];
        let record = record(&[legacy_x.clone(), legacy_y.clone()], &chunks, 5);
        let mut check = HistoryCheck::default();
        for row in [&legacy_x, &legacy_y, &chunks[0], &chunks[1], &record] {
            check.push(row).unwrap();
        }
        let verdict = check.finish();
        assert!(verdict.conflicts.is_empty(), "{:?}", verdict.conflicts);
        assert!(verdict.legacy_disagreement.is_none());
        assert_eq!(verdict.admitted_by[&claim("X")], Uuid::from_u128(1));
        assert_eq!(verdict.admitted_by[&claim("Y")], Uuid::from_u128(2));
    }

    /// With no live claims the record is the activation's only row, at
    /// ordinal 0, and it resets the legacy replay.
    #[test]
    fn a_record_with_no_chunks_stands_alone_and_resets() {
        let legacy = AuditRow {
            asserted_claims: vec![claim("X")],
            ..row(1, "admit_x")
        };
        let record = record(std::slice::from_ref(&legacy), &[], 2);
        assert_eq!(record.act_ordinal, Some(0));
        let mut check = HistoryCheck::default();
        check.push(&legacy).unwrap();
        check.push(&record).unwrap();
        let verdict = check.finish();
        assert!(verdict.conflicts.is_empty(), "{:?}", verdict.conflicts);
        assert!(verdict.admitted_by.is_empty());
        let disagreement = verdict
            .legacy_disagreement
            .expect("X was replayed, not kept");
        assert_eq!(disagreement.only_in_replay.len(), 1);
        assert!(disagreement.only_in_baseline.is_empty());
    }
}
