//! How a replay of the audit log moves the admitted state, one row at a
//! time. Every replay advances through here, so the state a reader
//! rebuilds does not depend on which reader rebuilt it. What a reader
//! makes of a row (a score, a coverage count, a rehearsal class) stays
//! with that reader.
//!
//! A row is one of four kinds. A legacy row, from before the canonical
//! order, is read from the claims it proposed to retract and admit; when
//! two writers overlapped, that can differ from what its commit did. The
//! activation's rows, its baseline chunks and then its record, replace
//! the state with the baseline: the first of them resets, each chunk
//! admits its claims, the record admits nothing. A row after it applies
//! the operations its commit performed.

use std::borrow::Cow;
use std::collections::HashMap;

use morpholog_core::{ClaimInstance, State};
use uuid::Uuid;

use crate::activation::{ACTIVATE, BASELINE};
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
    /// A chunk of the baseline. `first` on the activation's first row,
    /// which resets everything before it.
    Baseline {
        operations: &'r [Operation],
        first: bool,
    },
    /// The activation record, after every chunk; `first` when there are
    /// none, and the record resets.
    Activation {
        first: bool,
    },
    Ordered {
        operations: &'r [Operation],
    },
}

impl<'r> EffectKind<'r> {
    /// The kind of a row named `name` at act ordinal `act_ordinal` with
    /// these lists and operations.
    pub(crate) fn of(
        name: &str,
        act_ordinal: Option<u32>,
        retracted: &'r [ClaimInstance],
        asserted: &'r [ClaimInstance],
        operations: Option<&'r [Operation]>,
    ) -> Self {
        let first = act_ordinal == Some(0);
        match operations {
            None => EffectKind::Legacy {
                retracted,
                asserted,
            },
            Some(_) if name == ACTIVATE => EffectKind::Activation { first },
            Some(operations) if name == BASELINE => EffectKind::Baseline { operations, first },
            Some(operations) => EffectKind::Ordered { operations },
        }
    }

    /// A row of the activation: a chunk or the record.
    pub(crate) fn is_control(&self) -> bool {
        matches!(
            self,
            EffectKind::Baseline { .. } | EffectKind::Activation { .. }
        )
    }
}

/// One step of a row's effect, in the order replay applies it.
pub(crate) enum Change<'r> {
    /// The activation begins: everything before it is replaced.
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
                self.act_ordinal,
                &self.retracted_claims,
                &self.asserted_claims,
                self.operations.as_deref(),
            ),
        }
    }

    /// A row of the activation, a chunk of its baseline or its record:
    /// a control record, not a transition of any programme.
    pub(crate) fn is_control(&self) -> bool {
        self.effect().kind.is_control()
    }
}

impl RowEffect<'_> {
    pub(crate) fn changes(&self) -> Vec<Change<'_>> {
        let reset = |first: bool| first.then_some(Change::Reset);
        match &self.kind {
            EffectKind::Legacy {
                retracted,
                asserted,
            } => retracted
                .iter()
                .map(|c| Change::Retract(Cow::Borrowed(c)))
                .chain(asserted.iter().map(|c| Change::Admit(Cow::Borrowed(c))))
                .collect(),
            EffectKind::Baseline { operations, first } => reset(*first)
                .into_iter()
                .chain(
                    operations
                        .iter()
                        .map(|op| Change::Admit(Cow::Owned(op.claim()))),
                )
                .collect(),
            EffectKind::Activation { first } => reset(*first).into_iter().collect(),
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
    /// insert-if-absent does. A baseline chunk carries each claim's own
    /// admitter, never the chunk's id.
    pub(crate) fn apply_to_provenance(&self, admitted_by: &mut HashMap<ClaimInstance, Uuid>) {
        if let EffectKind::Baseline { operations, first } = &self.kind {
            if *first {
                admitted_by.clear();
            }
            for op in *operations {
                if let Some(by) = op.asserted_in {
                    admitted_by.entry(op.claim()).or_insert(by);
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
