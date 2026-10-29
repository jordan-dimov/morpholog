//! How a replay of the audit log moves the admitted state, one row at a
//! time. Every replay advances through here, so the state a reader
//! rebuilds does not depend on which reader rebuilt it. What a reader
//! makes of a row (a score, a coverage count, a rehearsal class) stays
//! with that reader.
//!
//! A row's effect is read from the claims it proposed to retract and
//! admit. When two writers overlapped, that can differ from what the
//! commit actually did to the claims table.

use std::collections::HashMap;

use morpholog_core::{ClaimInstance, State};
use uuid::Uuid;

use crate::audit::AuditRow;

/// What replay takes one row to have done: the claims it proposed to
/// retract and admit.
pub(crate) struct RowEffect<'r> {
    pub(crate) transition_id: Uuid,
    pub(crate) retracted: &'r [ClaimInstance],
    pub(crate) asserted: &'r [ClaimInstance],
}

/// One step of a row's effect, in the order the commit applied it.
pub(crate) enum Change<'r> {
    Retract(&'r ClaimInstance),
    Admit(&'r ClaimInstance),
}

impl AuditRow {
    pub(crate) fn effect(&self) -> RowEffect<'_> {
        RowEffect {
            transition_id: self.transition_id,
            retracted: &self.retracted_claims,
            asserted: &self.asserted_claims,
        }
    }
}

impl RowEffect<'_> {
    /// Retractions first, then admissions, as the commit applies them.
    pub(crate) fn changes(&self) -> impl Iterator<Item = Change<'_>> {
        self.retracted
            .iter()
            .map(Change::Retract)
            .chain(self.asserted.iter().map(Change::Admit))
    }

    pub(crate) fn apply_to(&self, state: &mut State) {
        state.apply(self.asserted, self.retracted);
    }

    /// Track which transition admitted each live claim: an admission
    /// counts only for a claim not already live, as the commit's
    /// insert-if-absent does.
    pub(crate) fn apply_to_provenance(&self, admitted_by: &mut HashMap<ClaimInstance, Uuid>) {
        for change in self.changes() {
            match change {
                Change::Retract(claim) => {
                    admitted_by.remove(claim);
                }
                Change::Admit(claim) => {
                    admitted_by
                        .entry(claim.clone())
                        .or_insert(self.transition_id);
                }
            }
        }
    }
}
