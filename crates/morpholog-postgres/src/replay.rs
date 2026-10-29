//! How a replay of the audit log moves the admitted state, one row at a
//! time. Every replay advances through here, so the state a reader
//! rebuilds does not depend on which reader rebuilt it. What a reader
//! makes of a row (a score, a coverage count, a rehearsal class) stays
//! with that reader.
//!
//! A row is one of three kinds. A legacy row, from before the canonical
//! order, is read from the claims it proposed to retract and admit; when
//! two writers overlapped, that can differ from what its commit did. The
//! activation record replaces the state with its baseline. A row after it
//! applies the operations its commit performed.

use std::borrow::Cow;
use std::collections::HashMap;

use morpholog_core::{ClaimInstance, State};
use uuid::Uuid;

use crate::activation::ACTIVATE;
use crate::audit::{AuditRow, Operation, OperationKind};

/// What replay takes one row to have done.
pub(crate) struct RowEffect<'r> {
    pub(crate) transition_id: Uuid,
    pub(crate) kind: EffectKind<'r>,
}

pub(crate) enum EffectKind<'r> {
    Legacy {
        retracted: &'r [ClaimInstance],
        asserted: &'r [ClaimInstance],
    },
    Activation {
        baseline: &'r [Operation],
    },
    Ordered {
        operations: &'r [Operation],
    },
}

impl<'r> EffectKind<'r> {
    /// The kind of a row named `name` with these lists and operations.
    pub(crate) fn of(
        name: &str,
        retracted: &'r [ClaimInstance],
        asserted: &'r [ClaimInstance],
        operations: Option<&'r [Operation]>,
    ) -> Self {
        match operations {
            None => EffectKind::Legacy {
                retracted,
                asserted,
            },
            Some(baseline) if name == ACTIVATE => EffectKind::Activation { baseline },
            Some(operations) => EffectKind::Ordered { operations },
        }
    }
}

/// One step of a row's effect, in the order replay applies it.
pub(crate) enum Change<'r> {
    /// The activation record: everything before it is replaced.
    Reset,
    Retract(Cow<'r, ClaimInstance>),
    Admit(Cow<'r, ClaimInstance>),
}

impl AuditRow {
    pub(crate) fn effect(&self) -> RowEffect<'_> {
        RowEffect {
            transition_id: self.transition_id,
            kind: EffectKind::of(
                self.transformation_name.as_str(),
                &self.retracted_claims,
                &self.asserted_claims,
                self.operations.as_deref(),
            ),
        }
    }

    /// The activation record is a control record, not a transition of
    /// any programme.
    pub(crate) fn is_activation(&self) -> bool {
        matches!(self.effect().kind, EffectKind::Activation { .. })
    }
}

impl RowEffect<'_> {
    pub(crate) fn changes(&self) -> Vec<Change<'_>> {
        match &self.kind {
            EffectKind::Legacy {
                retracted,
                asserted,
            } => retracted
                .iter()
                .map(|c| Change::Retract(Cow::Borrowed(c)))
                .chain(asserted.iter().map(|c| Change::Admit(Cow::Borrowed(c))))
                .collect(),
            EffectKind::Activation { baseline } => std::iter::once(Change::Reset)
                .chain(
                    baseline
                        .iter()
                        .map(|op| Change::Admit(Cow::Owned(op.claim()))),
                )
                .collect(),
            EffectKind::Ordered { operations } => operations
                .iter()
                .filter_map(|op| match op.op {
                    OperationKind::Del => Some(Change::Retract(Cow::Owned(op.claim()))),
                    OperationKind::Ins => Some(Change::Admit(Cow::Owned(op.claim()))),
                    OperationKind::Noop => None,
                })
                .collect(),
        }
    }

    pub(crate) fn apply_to(&self, state: &mut State) {
        if let EffectKind::Legacy {
            retracted,
            asserted,
        } = &self.kind
        {
            state.apply(asserted, retracted);
            return;
        }
        for change in self.changes() {
            match change {
                Change::Reset => *state = State::default(),
                Change::Retract(claim) => state.apply(&[], std::slice::from_ref(&*claim)),
                Change::Admit(claim) => state.apply(std::slice::from_ref(&*claim), &[]),
            }
        }
    }

    /// Track which transition admitted each live claim: an admission
    /// counts only for a claim not already live, as the commit's
    /// insert-if-absent does. The baseline carries its own provenance.
    pub(crate) fn apply_to_provenance(&self, admitted_by: &mut HashMap<ClaimInstance, Uuid>) {
        if let EffectKind::Activation { baseline } = &self.kind {
            admitted_by.clear();
            for op in *baseline {
                if let Some(by) = op.asserted_in {
                    admitted_by.insert(op.claim(), by);
                }
            }
            return;
        }
        for change in self.changes() {
            match change {
                Change::Reset => admitted_by.clear(),
                Change::Retract(claim) => {
                    admitted_by.remove(&*claim);
                }
                Change::Admit(claim) => {
                    admitted_by
                        .entry(claim.into_owned())
                        .or_insert(self.transition_id);
                }
            }
        }
    }
}
