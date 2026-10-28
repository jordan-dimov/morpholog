//! How one committed audit row moves the admitted state. Every replay of
//! the log advances through here, so the state a reader rebuilds does not
//! depend on which reader rebuilt it. What a reader makes of a row (a
//! score, a coverage count, a rehearsal class) stays with that reader.

use std::collections::HashMap;

use morpholog_core::{ClaimInstance, State};
use uuid::Uuid;

use crate::audit::AuditRow;

/// What one row did to the admitted claims.
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
