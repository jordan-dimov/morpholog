//! Morpholog PostgreSQL persistence adapter.
//!
//! Runs the synchronous [`morpholog_core`] kernel against PostgreSQL:
//! one `SERIALIZABLE` transaction per proposal, invariants compiled to
//! SQL where they can be, and the audit log, outbox and evidence packs
//! around it.
//!
//! See `crates/morpholog-core/sql/schema.sql` for the canonical schema
//! and `docs/scope-and-ambition.md` for the runtime's positioning.

mod sql_views;
pub mod testing;

pub use sql_views::{RenderedViews, ViewRefusal, render_views};

pub use sqlx::PgPool;

mod actor_policy;
mod as_of;
mod attestation;
mod audit;
mod audit_pages;
mod checkpoints;
mod claims;
mod compiled;
#[cfg(test)]
mod compiled_differential;
#[cfg(test)]
mod compiled_plan_shapes;
mod derived;
mod error;
mod indexes;
mod keys;
mod merkle;
mod migrations;
mod outbox;
mod pack;
mod prefix_verify;
mod program;
mod propose;
mod provision;
mod rejections;
mod role_rebindings;
#[cfg(test)]
mod scope_differential;
mod score;
mod signing;
mod sql_quote;
mod transact;
mod txn;
mod verify;
pub mod wire_time;
mod witnesses;

// The whole public surface, flat at the crate root.
pub use actor_policy::{
    AUTHORITY_PREDICATE, PolicyDeclarationError, RESTRICTED_PREDICATE, validate_declarations,
};
pub use as_of::{
    list_claims_at, list_claims_at_for_predicates, reconstruct_state_at,
    resolve_transition_at_or_before,
};
pub use attestation::{ActorAttestation, AuditAttestation, Proposal};
pub use audit::{
    AuditRow, AuditTail, audit_cursor_for, audit_resume_watermark, begin_audit_tail,
    list_audit_rows, list_audit_rows_page,
};
pub use checkpoints::{
    Checkpoint, CheckpointOutcome, CheckpointSigner, SignaturePolicy, SignaturePolicyViolation,
    TreeHeadSignature, TreeVerification, Witness, WitnessScheme, attach_witness, create_checkpoint,
    load_checkpoint, verify_audit_tree, verify_audit_tree_under, verify_audit_tree_with_chain,
    with_anchor_signatures,
};
pub use claims::{
    ClaimFilter, list_claims, list_claims_for_predicates, list_claims_where, load_scoped_state,
};
pub use compiled::{CompileReason, CompileRefusal};
pub use derived::{RefreshSummary, list_derived, list_derived_at, refresh_derived};
pub use error::PgError;
pub use indexes::{IndexAction, IndexPlanEntry, ProvisionReport, plan_indexes, provision_indexes};
pub use merkle::{Digest, DigestError};
pub use migrations::{
    MigrationRef, MigrationReport, apply_migrations, head_version, migration_status,
};
pub use outbox::{
    CompensationSpec, Deliverer, DeliveryOutcome, OutboxRow, OutboxStatus, OutboxUpdate,
    ProcessOutcome, begin_compensation, claim_pending_outbox_row, complete_compensation,
    earliest_pending_retry, list_outbox_rows, list_pending_outbox, mark_compensation_failed,
    mark_outbox_delivered, mark_outbox_failed, mark_outbox_transient_attempt,
    process_one_outbox_row, record_compensation, release_outbox_claim,
};
pub use pack::{
    EvidencePack, PackError, PackManifest, PrefixExport, PrefixPackManifest, PrefixStreamReport,
    RowInclusionProof, SelectiveEvidencePack, SelectivePackManifest, SelectiveVerification,
    WindowEvidencePack, WindowPackManifest, WindowStart, WindowVerification, begin_prefix_export,
    export_pack, export_selective, export_window, pack_format_version, pack_role_rebindings,
    read_prefix_stream, streamed_pack_version, verify_pack, verify_prefix_stream, verify_selective,
    verify_window,
};
pub use program::{InvariantPlan, PgProgram};
pub use propose::{
    AuditedInvariantCheck, PgProposalOutcome, PgTracedOutcome, ProposalPhases,
    RejectionStateOutcome, TimedProposalOutcome, compute_idempotency_key, propose_against_pg,
    propose_against_pg_timed, propose_against_pg_with_rejection_state,
    propose_against_pg_with_trace,
};
pub(crate) use provision::least_privilege_roles_exist;
pub use provision::{
    InitOutcome, READER_ROLE, WRITER_ROLE, drop_schema, initialise_schema,
    provision_least_privilege, redact_database_url, single_connection_pool, with_default_user,
    with_user,
};
pub use rejections::{RejectionRow, list_rejection_rows};
pub use role_rebindings::{RebindingScope, RoleRebinding, RoleRebindings};
pub use score::{
    SplitBoundary, score_candidate, score_candidate_against_pack, score_candidate_against_packs,
    score_candidate_against_packs_lazily,
};
pub use signing::{
    SigningError, TreeHead, generate_signing_key, parse_public_key, parse_signature,
    render_public_key, render_signature, sign_tree_head, signing_key_from_pem, signing_key_to_pem,
    tree_head_signing_bytes, tree_head_witness_bytes, verify_tree_head,
};
pub use transact::{AtomicAct, PgAtomicOutcome, propose_all_against_pg};
pub use verify::{
    VerifyOutcome, VerifyReport, ViewsVerification, coverage_replay, verify_replay, verify_views,
};
pub use witnesses::{
    CheckpointWitnesses, PackVerdict, PackVerificationReport, WitnessAnchors, WitnessStanding,
    WitnessVerdict, WitnessesReport, witnesses_report,
};
