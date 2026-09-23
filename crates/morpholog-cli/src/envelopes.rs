//! The report shapes the binary owns, as opposed to kernel or adapter
//! structs it serializes as-is.
//!
//! Fields are declared in alphabetical order. Field order is wire order,
//! and the goldens go through `serde_json::Value`, which sorts keys.

use morpholog_core::WitnessBinding;
use serde::Serialize;

/// `check --json`: the uniform findings report.
#[derive(Serialize)]
pub struct CheckReport {
    pub diagnostics: Vec<CheckDiagnostic>,
    pub file: String,
}

/// One finding in `check --json`. Byte offsets and 1-based
/// line/column are present when the finding has a source anchor; a
/// finding without one (a generated discipline invariant) carries
/// only severity and message.
#[derive(Serialize)]
pub struct CheckDiagnostic {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub message: String,
    pub severity: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<usize>,
}

impl CheckDiagnostic {
    pub fn new(
        severity: &'static str,
        message: String,
        span: Option<morpholog_surface::Span>,
        source: &str,
    ) -> Self {
        let (line, column) = match &span {
            Some(s) => {
                let (l, c) = morpholog_surface::line_col(source, s.start);
                (Some(l), Some(c))
            }
            None => (None, None),
        };
        Self {
            column,
            end: span.as_ref().map(|s| s.end),
            line,
            message,
            severity,
            start: span.as_ref().map(|s| s.start),
        }
    }
}

/// `hash`: the canonical rules-identity hash of a programme.
#[derive(Serialize)]
pub struct HashReport {
    pub hash: String,
    pub program: String,
}

/// `session`: the ready line, the only line a session emits unprompted.
/// `model_hash` is the rules hash the session pinned, for staleness checks.
/// `protocol` versions the wire, separately from the binary.
#[derive(Serialize)]
pub struct SessionReady {
    pub model_hash: String,
    pub morpholog_version: &'static str,
    pub program: String,
    pub protocol: u32,
    pub status: &'static str,
}

impl SessionReady {
    pub fn new(model_hash: String, program: String) -> Self {
        Self {
            model_hash,
            morpholog_version: env!("CARGO_PKG_VERSION"),
            program,
            protocol: 1,
            status: "ready",
        }
    }
}

/// The per-row error receipt of `propose --batch` and `session`. The
/// stable `code` lets a caller decide on a retry without parsing prose.
/// `row` is the 1-based input line or request number, as on propose
/// receipts.
#[derive(Serialize)]
pub struct ErrorReceipt {
    pub code: ErrorCode,
    pub error: String,
    pub row: u64,
    pub status: &'static str,
}

impl ErrorReceipt {
    pub fn new(code: ErrorCode, error: String, row: u64) -> Self {
        Self {
            code,
            error,
            row,
            status: "error",
        }
    }
}

/// `transact`'s error object: an error for the whole batch. Same stable
/// `code` as a receipt, but no `row`, since the batch is one request.
#[derive(Serialize)]
pub struct AtomicError {
    pub code: ErrorCode,
    pub error: String,
    pub status: &'static str,
}

impl AtomicError {
    pub fn new(code: ErrorCode, error: String) -> Self {
        Self {
            code,
            error,
            status: "error",
        }
    }
}

/// The closed set of per-row failure codes a batch or session can return.
/// Only `serialization_failure` is safe to re-submit; the rest describe the
/// row itself. Operational failures abort the run instead.
///
/// One list builds both the enum and the `ALL` slice a contract test
/// walks, so the published set cannot miss a code.
macro_rules! error_codes {
    ($($variant:ident),+ $(,)?) => {
        #[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
        #[serde(rename_all = "snake_case")]
        pub enum ErrorCode {
            $($variant),+
        }

        impl ErrorCode {
            /// Every code, so a test can hold `result.json` to what
            /// the binary can actually emit.
            pub const ALL: &'static [ErrorCode] =
                &[$(ErrorCode::$variant),+];
        }
    };
}

/// The codes a proposal row can fail with, in a batch or a session: every
/// code except the session-only `unknown_operation`. A type, so a proposal
/// row cannot get a code outside the published `propose_error_code` set; a
/// test holds the schema to `ALL`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProposeCode {
    ActorAssertionUnauthorised,
    /// The database connection failed while COMMIT was in flight, so
    /// the runtime cannot prove whether the proposal took effect. Read
    /// the record before re-submitting.
    CommitOutcomeUnknown,
    DuplicateIntent,
    InvalidArguments,
    InvalidRequest,
    KernelError,
    /// The database refused or failed before the proposal was durably
    /// recorded: nothing changed. Re-submit once the cause is fixed.
    NotCommitted,
    SerializationFailure,
    UnknownTransformation,
}

impl ProposeCode {
    pub const ALL: &'static [ProposeCode] = &[
        ProposeCode::ActorAssertionUnauthorised,
        ProposeCode::CommitOutcomeUnknown,
        ProposeCode::DuplicateIntent,
        ProposeCode::InvalidArguments,
        ProposeCode::InvalidRequest,
        ProposeCode::KernelError,
        ProposeCode::NotCommitted,
        ProposeCode::SerializationFailure,
        ProposeCode::UnknownTransformation,
    ];
}

impl From<ProposeCode> for ErrorCode {
    fn from(code: ProposeCode) -> Self {
        match code {
            ProposeCode::ActorAssertionUnauthorised => ErrorCode::ActorAssertionUnauthorised,
            ProposeCode::CommitOutcomeUnknown => ErrorCode::CommitOutcomeUnknown,
            ProposeCode::DuplicateIntent => ErrorCode::DuplicateIntent,
            ProposeCode::InvalidArguments => ErrorCode::InvalidArguments,
            ProposeCode::InvalidRequest => ErrorCode::InvalidRequest,
            ProposeCode::KernelError => ErrorCode::KernelError,
            ProposeCode::NotCommitted => ErrorCode::NotCommitted,
            ProposeCode::SerializationFailure => ErrorCode::SerializationFailure,
            ProposeCode::UnknownTransformation => ErrorCode::UnknownTransformation,
        }
    }
}

error_codes!(
    ActorAssertionUnauthorised,
    CommitOutcomeUnknown,
    DuplicateIntent,
    InvalidArguments,
    InvalidRequest,
    KernelError,
    NotCommitted,
    SerializationFailure,
    UnknownOperation,
    UnknownTransformation,
);

/// `init`: day-zero provisioning outcome.
#[derive(Serialize)]
pub struct InitReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub least_privilege: Option<LeastPrivilegeReport>,
    pub schema: &'static str,
    pub status: &'static str,
}

/// The `--least-privilege` floor, as applied: the two group roles, and
/// the membership grants only the operator can decide (which login
/// roles the runtime and its read-only consumers actually use).
#[derive(Serialize)]
pub struct LeastPrivilegeReport {
    pub next_steps: Vec<String>,
    pub reader_role: &'static str,
    pub writer_role: &'static str,
}

impl LeastPrivilegeReport {
    pub fn applied() -> Self {
        let writer = morpholog_postgres::WRITER_ROLE;
        let reader = morpholog_postgres::READER_ROLE;
        Self {
            next_steps: vec![
                format!("GRANT {writer} TO <the runtime's login role>;"),
                format!("GRANT {reader} TO <each reporting or projection login role>;"),
                "GRANT pg_read_all_stats TO <each role that tails the audit>; \
                 -- the resume watermark reads pg_stat_activity"
                    .to_string(),
            ],
            reader_role: reader,
            writer_role: writer,
        }
    }
}

/// `refresh derived`: the published read-model generation.
///
/// The snapshot pair is the latest audit transition the refresh saw. It is
/// a rough freshness marker, not a resume cursor: a writer still in flight
/// is missed until the next refresh (`inspect audit` resumes losslessly).
/// Both are absent on an empty ledger. Timings go to stderr, not here.
#[derive(Serialize)]
pub struct RefreshDerivedReport {
    pub derived_claim_count: usize,
    pub derived_predicate_count: usize,
    pub model_hash: String,
    pub refresh_id: uuid::Uuid,
    pub source_claim_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(with = "morpholog_postgres::wire_time::option")]
    pub source_snapshot_committed_at: Option<jiff::Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_snapshot_transition_id: Option<uuid::Uuid>,
}

impl From<&morpholog_postgres::RefreshSummary> for RefreshDerivedReport {
    fn from(s: &morpholog_postgres::RefreshSummary) -> Self {
        // Both come from one audit row. A one-sided pair is an upstream
        // bug, not an empty ledger.
        let snapshot = match (
            s.source_snapshot_transition_id,
            s.source_snapshot_committed_at,
        ) {
            (Some(tid), Some(at)) => Some((tid, at)),
            (None, None) => None,
            _ => unreachable!("refresh snapshot coordinates must be paired"),
        };
        Self {
            derived_claim_count: s.derived_claim_count,
            derived_predicate_count: s.derived_predicate_count,
            model_hash: s.model_hash.clone(),
            refresh_id: s.refresh_id,
            source_claim_count: s.source_claim_count,
            source_snapshot_committed_at: snapshot.map(|(_, at)| at),
            source_snapshot_transition_id: snapshot.map(|(tid, _)| tid),
        }
    }
}

/// The `propose --trace` envelope: `{result, trace}`. Generic so the
/// adapter's outcome and trace serialize by reference.
#[derive(Serialize)]
pub struct Traced<R, T> {
    pub result: R,
    pub trace: T,
}

/// The errored `result` inside a traced envelope: the transformation raised
/// a kernel error. The constructor sets `status`, so it cannot be misspelled.
#[derive(Serialize)]
pub struct TracedError {
    error: String,
    status: &'static str,
}

impl TracedError {
    pub fn new(error: String) -> Self {
        Self {
            error,
            status: "errored",
        }
    }
}

/// A rejection carrying the same-snapshot explanation
/// (`propose --explain-on-reject`), single and batch paths alike. The
/// constructor owns the `status` discriminator.
#[derive(Serialize)]
pub struct RejectedWithExplanation<'a, E> {
    explanation: E,
    reason: &'a str,
    /// The refused rule's stable name. Absent when a gate has no name;
    /// never the rendered expression, which a rewording would change.
    #[serde(skip_serializing_if = "Option::is_none")]
    rule: Option<&'a str>,
    status: &'static str,
    /// The refused rule's offending values. An operator diagnosing a
    /// refusal needs them most on this path.
    #[serde(skip_serializing_if = "<[WitnessBinding]>::is_empty")]
    witness: &'a [WitnessBinding],
}

impl<'a, E> RejectedWithExplanation<'a, E> {
    pub fn new(
        reason: &'a str,
        rule: Option<&'a str>,
        witness: &'a [WitnessBinding],
        explanation: E,
    ) -> Self {
        Self {
            explanation,
            reason,
            rule,
            status: "rejected",
            witness,
        }
    }
}

/// One claim decoded to the named form: field-keyed bare values under
/// the declared predicate vocabulary (the read-side mirror of
/// `--args-named`).
#[derive(Serialize)]
pub struct NamedClaim {
    pub args: serde_json::Map<String, serde_json::Value>,
    pub predicate: morpholog_core::PredicateName,
}

/// The `--named` audit row: the tagged row with its two claim arrays
/// replaced by named claims; everything else is unchanged. Shared with the
/// contract test so there is one definition.
pub fn audit_row_named(
    row: &morpholog_postgres::AuditRow,
    asserted: Vec<NamedClaim>,
    retracted: Vec<NamedClaim>,
) -> serde_json::Result<serde_json::Value> {
    let serde_json::Value::Object(mut obj) = serde_json::to_value(row)? else {
        return Err(serde::ser::Error::custom(
            "an AuditRow serialises as an object",
        ));
    };
    obj.insert(
        "asserted_claims".to_string(),
        serde_json::to_value(asserted)?,
    );
    obj.insert(
        "retracted_claims".to_string(),
        serde_json::to_value(retracted)?,
    );
    Ok(serde_json::Value::Object(obj))
}
