//! How an actor identity was established, and the proposal shape that
//! carries it into every durable commit path.
//!
//! The kernel's `Transition.actor` says WHO proposed; the attestation
//! says HOW the runtime came to believe it. The durable adapter accepts
//! only attested proposals, so no commit path can record an actor
//! without also recording its lineage.

use morpholog_core::{EvalValue, Subject, TransformationName, Transition};
use serde::{Deserialize, Serialize};

/// How the caller established the actor identity it is proposing under.
///
/// Gateway attestation is an assertion: the connected application names
/// the actor, and the runtime records which PostgreSQL-authenticated
/// role vouched for it. It proves who asserted, never that the named
/// actor authorised anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorAttestation {
    Gateway { actor: Subject },
}

/// A proposed change plus the attestation of its actor - what the
/// durable commit paths accept in place of a bare kernel
/// [`Transition`], so actor lineage is unbypassable by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub transformation_name: TransformationName,
    pub args: Vec<EvalValue>,
    pub attestation: ActorAttestation,
}

impl Proposal {
    /// A gateway-attested proposal from a kernel transition: the
    /// caller asserts the transition's actor, and the runtime records
    /// which authenticated role did the asserting.
    pub fn gateway(transition: &Transition) -> Self {
        Self {
            transformation_name: transition.transformation_name.clone(),
            args: transition.args.clone(),
            attestation: ActorAttestation::Gateway {
                actor: transition.actor.clone(),
            },
        }
    }

    /// The actor the attestation establishes.
    pub fn actor(&self) -> &Subject {
        match &self.attestation {
            ActorAttestation::Gateway { actor } => actor,
        }
    }

    /// The kernel transition this proposal evaluates as. The actor is
    /// derived from the attestation - there is no second place to
    /// supply it, so the two can never disagree.
    pub(crate) fn transition(&self) -> Transition {
        Transition {
            transformation_name: self.transformation_name.clone(),
            args: self.args.clone(),
            actor: self.actor().clone(),
        }
    }
}

/// The attestation lineage as the audit row stores it. It does not repeat
/// the actor: the row's `actor` column is the single source. Decoding is
/// strict: an unknown `mode` is an error, never a value that reaches
/// hashing or display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum AuditAttestation {
    /// The login role of the proposing connection asserted the actor.
    /// The adapter reads it from `session_user` inside the committing
    /// transaction; the caller never supplies it.
    Gateway {
        authenticated_by: String,
        /// The role's OID when it asserted: which incarnation of the name,
        /// since a dropped role's name can be created again. Local to the
        /// cluster, not a permanent identity. Absent on rows written
        /// before it was recorded, which then hash exactly as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authenticated_by_oid: Option<u32>,
    },
}
