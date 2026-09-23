//! Which login-role names changed identity within a verified history.
//!
//! A gateway attestation names the role that asserted an actor, and from
//! the release that added it, that role's OID. A role can be dropped and
//! a new one created under the same name; the OID tells the two apart. A
//! change is a finding, not tampering: recreating a role can be
//! legitimate. It never alters a verification verdict, and it is reported
//! only over rows an intact verdict established, since a finding drawn
//! from rows that failed verification would rest on untrusted evidence.

use std::collections::HashMap;

use serde::Serialize;
use uuid::Uuid;

use crate::AuditAttestation;
use crate::audit::AuditRow;

/// The rebinding finding for one verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RoleRebindings {
    /// Compared over rows an intact verdict established.
    Evaluated {
        /// What the rows are, which bounds what a change can claim.
        scope: RebindingScope,
        /// Rows whose attestation carries a role OID, and so were compared.
        rows_with_oid: u64,
        /// Rows without one (written before it was recorded, or before
        /// attestation existed), which cannot show a change.
        rows_without_oid: u64,
        changes: Vec<RoleRebinding>,
    },
    /// The verdict did not establish the rows, so nothing is reported
    /// from them.
    NotEvaluated,
}

/// What the compared rows are. A complete prefix is every row up to a
/// checkpoint; a window every row between two; a selective pack only the
/// rows chosen for disclosure, so rows between two it shows may be absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RebindingScope {
    CompletePrefix,
    Window,
    Selective,
}

/// One role name seen under a new OID. The transitions are the last and
/// first ones observed in the compared rows, not necessarily the last and
/// first in the log, since a window or a selective pack shows only part of
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleRebinding {
    pub role: String,
    pub previous_oid: u32,
    pub last_observed_transition: Uuid,
    pub new_oid: u32,
    pub first_observed_transition: Uuid,
}

/// Folds audit rows, in log order, into the finding.
#[derive(Debug, Default)]
pub(crate) struct RebindingFold {
    current: HashMap<String, (u32, Uuid)>,
    rows_with_oid: u64,
    rows_without_oid: u64,
    changes: Vec<RoleRebinding>,
}

impl RebindingFold {
    pub(crate) fn observe(&mut self, row: &AuditRow) {
        let Some(AuditAttestation::Gateway {
            authenticated_by,
            authenticated_by_oid: Some(oid),
        }) = &row.attestation
        else {
            self.rows_without_oid += 1;
            return;
        };
        self.rows_with_oid += 1;
        let seen = (*oid, row.transition_id);
        if let Some((previous_oid, last)) = self.current.insert(authenticated_by.clone(), seen)
            && previous_oid != *oid
        {
            self.changes.push(RoleRebinding {
                role: authenticated_by.clone(),
                previous_oid,
                last_observed_transition: last,
                new_oid: *oid,
                first_observed_transition: row.transition_id,
            });
        }
    }

    /// The finding, if `established`; otherwise [`RoleRebindings::NotEvaluated`].
    pub(crate) fn finish(self, scope: RebindingScope, established: bool) -> RoleRebindings {
        if !established {
            return RoleRebindings::NotEvaluated;
        }
        RoleRebindings::Evaluated {
            scope,
            rows_with_oid: self.rows_with_oid,
            rows_without_oid: self.rows_without_oid,
            changes: self.changes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpholog_core::Subject;

    fn row(n: u128, role: &str, oid: Option<u32>) -> AuditRow {
        AuditRow {
            transition_id: Uuid::from_u128(n),
            transformation_name: "post".into(),
            arguments: vec![],
            actor: Subject::from("alex"),
            invariant_epoch: 1,
            invariants_checked: vec![],
            asserted_claims: vec![],
            retracted_claims: vec![],
            emitted_intents: vec![],
            committed_at: "2026-09-23T12:00:00Z".parse().unwrap(),
            attestation: Some(AuditAttestation::Gateway {
                authenticated_by: role.to_string(),
                authenticated_by_oid: oid,
            }),
            parameters: None,
        }
    }

    fn fold(rows: &[AuditRow]) -> RoleRebindings {
        let mut fold = RebindingFold::default();
        for r in rows {
            fold.observe(r);
        }
        fold.finish(RebindingScope::CompletePrefix, true)
    }

    /// Rows without an OID are counted but never compared, so they cannot
    /// manufacture a change; two roles are tracked apart; a name that goes
    /// back to an earlier OID is a change each time.
    #[test]
    fn the_fold_compares_only_rows_with_an_oid_and_each_role_on_its_own() {
        let RoleRebindings::Evaluated {
            rows_with_oid,
            rows_without_oid,
            changes,
            ..
        } = fold(&[
            row(1, "a", None),
            row(2, "a", Some(10)),
            row(3, "b", Some(20)),
            row(4, "a", None),
            row(5, "a", Some(11)),
            row(6, "b", Some(20)),
            row(7, "a", Some(10)),
        ])
        else {
            panic!("established rows are evaluated");
        };
        assert_eq!((rows_with_oid, rows_without_oid), (5, 2));
        let seen: Vec<_> = changes
            .iter()
            .map(|c| (c.role.as_str(), c.previous_oid, c.new_oid))
            .collect();
        assert_eq!(seen, [("a", 10, 11), ("a", 11, 10)]);
        assert_eq!(changes[0].last_observed_transition, Uuid::from_u128(2));
        assert_eq!(changes[0].first_observed_transition, Uuid::from_u128(5));
    }
}
