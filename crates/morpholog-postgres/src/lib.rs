//! Morpholog PostgreSQL persistence adapter.
//!
//! Thin async I/O layer around the existing synchronous
//! [`morpholog_core::propose`] kernel. The kernel itself is unchanged.
//!
//! See `crates/morpholog-core/sql/schema.sql` for the canonical schema
//! and `docs/scope-and-ambition.md` for the runtime's positioning.

mod sql_views;
pub mod testing;

pub use sql_views::{RenderedViews, ViewRefusal, render_views};

pub use sqlx::PgPool;

mod as_of;
mod attestation;
mod audit;
mod checkpoints;
mod claims;
mod derived;
mod error;
mod keys;
mod merkle;
mod outbox;
mod pack;
mod propose;
mod provision;
mod rejections;
mod score;
mod signing;
mod sql_quote;
mod txn;
mod verify;

// Re-export the full public surface so `morpholog_postgres::X` paths are unchanged.
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
    Checkpoint, CheckpointOutcome, CheckpointSigner, TreeHeadSignature, TreeVerification,
    create_checkpoint, first_unsigned_checkpoint_size, verify_audit_tree,
};
pub use claims::{list_claims, list_claims_for_predicates, load_scoped_state};
pub use derived::{RefreshSummary, list_derived, list_derived_at, refresh_derived};
pub use error::PgError;
pub use outbox::{
    CompensationSpec, Deliverer, DeliveryOutcome, OutboxRow, OutboxUpdate, ProcessOutcome,
    begin_compensation, claim_pending_outbox_row, complete_compensation, earliest_pending_retry,
    list_outbox_rows, list_pending_outbox, mark_compensation_failed, mark_outbox_delivered,
    mark_outbox_failed, mark_outbox_transient_attempt, process_one_outbox_row, record_compensation,
    release_outbox_claim,
};
pub use pack::{
    EvidencePack, PackError, PackManifest, RowInclusionProof, SelectiveEvidencePack,
    SelectivePackManifest, SelectiveVerification, WindowEvidencePack, WindowPackManifest,
    WindowStart, WindowVerification, export_pack, export_selective, export_window, verify_pack,
    verify_selective, verify_window,
};
pub use propose::{
    AuditedInvariantCheck, PgProposalOutcome, PgTracedOutcome, RejectionStateOutcome,
    compute_idempotency_key, propose_against_pg, propose_against_pg_with_rejection_state,
    propose_against_pg_with_trace,
};
pub use provision::{
    InitOutcome, READER_ROLE, WRITER_ROLE, drop_schema, initialise_schema,
    provision_least_privilege, redact_database_url, with_default_user, with_user,
};
pub use rejections::{RejectionRow, list_rejection_rows};
pub use score::{
    SplitBoundary, score_candidate, score_candidate_against_pack, score_candidate_against_packs,
};
pub use signing::{
    SigningError, TreeHead, generate_signing_key, parse_public_key, parse_signature,
    render_public_key, render_signature, sign_tree_head, signing_key_from_pem, signing_key_to_pem,
    tree_head_signing_bytes, verify_tree_head,
};
pub use verify::{
    VerifyOutcome, VerifyReport, ViewsVerification, coverage_replay, verify_replay, verify_views,
};
