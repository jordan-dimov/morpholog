//! The CLI-local envelope shapes: report surfaces the binary owns
//! (rather than re-serializing a kernel or adapter struct).
//!
//! Field order is wire order. The golden pins normalize through
//! `serde_json::Value`, whose object keys sort alphabetically, while
//! the binary serializes these structs directly - so fields are
//! declared in alphabetical order to keep the two byte-identical.

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

/// `session`: the ready line - the first and only unprompted line a
/// session emits. Carries the staleness token (`model_hash` is the
/// canonical rules-identity hash the programme was pinned at) and the
/// protocol number, which is the wire's version, distinct from the
/// binary's.
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

/// `session`: the per-request error receipt. Unlike the batch error
/// receipt it carries a stable `code`, because a session caller
/// deciding whether a retry is safe must never parse English prose.
/// `row` is the 1-based request line number, the same counter the
/// propose receipts carry.
#[derive(Serialize)]
pub struct SessionErrorReceipt {
    pub code: SessionErrorCode,
    pub error: String,
    pub row: u64,
    pub status: &'static str,
}

impl SessionErrorReceipt {
    pub fn new(code: SessionErrorCode, error: String, row: u64) -> Self {
        Self {
            code,
            error,
            row,
            status: "error",
        }
    }
}

/// The closed set of per-request failure codes a session can answer
/// with. `serialization_failure` is the one a caller may re-submit on
/// (retries stay the caller's); the rest describe the request itself.
/// Operational failures never become receipts - the session aborts.
/// One list, two products: the enum and the slice a contract test
/// walks. Declaring a variant anywhere else is impossible, so a code
/// the binary can emit cannot go missing from the published set - a
/// hand-kept array would compile happily while the enum grew past it.
macro_rules! session_error_codes {
    ($($variant:ident),+ $(,)?) => {
        #[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
        #[serde(rename_all = "snake_case")]
        pub enum SessionErrorCode {
            $($variant),+
        }

        impl SessionErrorCode {
            /// Every code, so a test can hold `result.json` to what
            /// the binary can actually emit.
            pub const ALL: &'static [SessionErrorCode] =
                &[$(SessionErrorCode::$variant),+];
        }
    };
}

session_error_codes!(
    ActorAssertionUnauthorised,
    DuplicateIntent,
    InvalidArguments,
    InvalidRequest,
    KernelError,
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
                format!(
                    "GRANT pg_read_all_stats TO <each role that tails the audit>; \
                     -- the resume watermark reads pg_stat_activity"
                ),
            ],
            reader_role: reader,
            writer_role: writer,
        }
    }
}

/// `refresh derived`: the published read-model generation, typed. The
/// snapshot pair is the latest audit transition visible in the
/// refresh's read snapshot - a coarse freshness marker, never a
/// lossless audit-resume cursor (a writer in flight at snapshot time
/// is excluded and folded in by the next refresh; lossless resume is
/// `inspect audit`). Absent together on an empty ledger. Timings stay
/// on stderr: operational colour, not contract.
#[derive(Serialize)]
pub struct RefreshDerivedReport {
    pub derived_claim_count: usize,
    pub derived_predicate_count: usize,
    pub model_hash: String,
    pub refresh_id: uuid::Uuid,
    pub source_claim_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_snapshot_committed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_snapshot_transition_id: Option<uuid::Uuid>,
}

impl From<&morpholog_postgres::RefreshSummary> for RefreshDerivedReport {
    fn from(s: &morpholog_postgres::RefreshSummary) -> Self {
        // The snapshot coordinates come from one audit row; a one-sided
        // pair is a bug upstream, and refusing beats reporting it as a
        // lawful empty ledger.
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

/// The `propose --trace` envelope: `{result, trace}`, exactly as the
/// contract pins it. Generic so the CLI serialises the adapter's
/// outcome and trace by reference without re-stating their types.
#[derive(Serialize)]
pub struct Traced<R, T> {
    pub result: R,
    pub trace: T,
}

/// The errored `result` inside a traced envelope: a transformation
/// that raised a kernel error mid-execution. The constructor owns the
/// `status` discriminator - a caller cannot misspell the tag.
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
    /// The refused rule's stable identifier. Absent when a gate has no
    /// name - never the rendered expression, so a caller reading this
    /// never gets a value a rewording can change.
    #[serde(skip_serializing_if = "Option::is_none")]
    rule: Option<&'a str>,
    status: &'static str,
    /// The refused rule's offending values. Carried here too because this
    /// is the path an operator diagnosing a refusal actually uses - the
    /// first cut destructured the outcome as `{ reason, .. }` and dropped
    /// them, so the one command built for diagnosis was the one that
    /// answered least.
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

/// The `--named` audit row: the tagged row's own serialization with
/// the two claim arrays replaced by named claims, everything else
/// byte-identical by construction. Shared by the binary and the
/// contract test so the projection has one definition.
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
