//! The CLI-local envelope shapes: report surfaces the binary owns
//! (rather than re-serializing a kernel or adapter struct).
//!
//! Field order is wire order. The golden pins normalize through
//! `serde_json::Value`, whose object keys sort alphabetically, while
//! the binary serializes these structs directly - so fields are
//! declared in alphabetical order to keep the two byte-identical.

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
