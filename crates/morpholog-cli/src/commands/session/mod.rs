//! `morpholog session` - a resident process over stdio. It validates the
//! programme once, holds one warm connection, and answers NDJSON requests
//! on stdin with the same envelopes the one-shot commands print, one line
//! per request, in order.
//!
//! The protocol is lockstep: one request in, one response out, no
//! correlation ids. A propose answers with the batch receipt (`row` is the
//! 1-based request line); reads answer with the claim arrays. A
//! per-request failure answers with an error receipt carrying a stable
//! `code`. A proposal's database failure is a receipt too:
//! `not_committed` when nothing was recorded, `commit_outcome_unknown`
//! when COMMIT failed without a verdict.
//!
//! `ping` answers with the ready line again after a database round
//! trip, and `audit` and `rejections` are the one-shot reads with the
//! same parameters, each answered as one JSON array. An `audit` request
//! opens its own tail, so its horizon and resume rules are the one-shot
//! tail's; `limit` caps the rows a resident loop holds in one answer.
//!
//! The process aborts, non-zero, only on a failure that cannot be a
//! receipt: a broken stream, an operational failure on a read, or a
//! proposal's receipt that could not be serialised after the adapter
//! returned. To a caller with a request in flight, an abort means the
//! outcome is unknown; the generated client reports it as such and never
//! retries silently. Everything the binary knows did not commit is a coded
//! receipt, never an abort.
//!
//! The programme is fixed at start; the ready line's `model_hash` tells a
//! caller which one. Editing the file never changes a running session, so
//! a new model means new sessions.

use anyhow::{Context, anyhow};
use std::io::{BufRead, Write};

use crate::SessionArgs;
use crate::commands::filter::FieldFilter;
use crate::commands::inspect::{
    audit_row_named_json, claims_rows, decode_claims_named, derived_rows, resolve_as_of,
};
use crate::commands::propose::{BatchRow, RowError, classify_pg_error, propose_row_outcome};
use crate::commands::transact::{Act, decode_acts};
use crate::commands::{parse_or_report, prepare_or_report};
use morpholog_cli::envelopes::{ErrorCode, ErrorReceipt, SessionReady};
use morpholog_core::PreparedProgram;
use morpholog_postgres::{PgError, PgPool, PgProgram};

/// A runaway guard, not a working limit. A longer request line aborts the
/// session: a half-read line cannot be resynchronised, so it cannot be a
/// receipt.
const MAX_REQUEST_LINE: usize = 64 * 1024 * 1024;

/// A per-request failure becomes a receipt and the session continues. An
/// operational failure aborts, so it never looks like an answered request.
enum SessionFailure {
    Request {
        code: ErrorCode,
        reason: anyhow::Error,
    },
    Operational(anyhow::Error),
}

impl SessionFailure {
    fn request(code: ErrorCode, reason: anyhow::Error) -> Self {
        SessionFailure::Request { code, reason }
    }
}

pub(crate) async fn run(args: SessionArgs) -> anyhow::Result<()> {
    // Before the ready line, failures behave as in one-shot commands:
    // diagnostics, then exit.
    let parsed = parse_or_report(&args.file)?;
    let program = morpholog_postgres::PgProgram::new(prepare_or_report(&parsed)?);
    let prepared = program.prepared();

    let pool = crate::commands::connect_single(&args.db.database_url).await?;

    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let ready = SessionReady::new(
        morpholog_core::format::canonical_hash(prepared.program()),
        prepared.program().name.clone(),
    );
    let ready = serde_json::to_value(&ready)?;
    write_line(&mut out, &ready)?;

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut line = String::new();
    let mut row: u64 = 0;
    loop {
        line.clear();
        if read_line_capped(&mut input, &mut line)? == 0 {
            break; // EOF: the clean shutdown.
        }
        row += 1;
        if line.trim().is_empty() {
            continue; // Skipped without a receipt, like a batch blank.
        }
        match handle_line(&args, &program, &pool, &ready, line.trim(), row).await {
            Ok(response) => write_line(&mut out, &response)?,
            Err(SessionFailure::Request { code, reason }) => {
                let receipt = ErrorReceipt::new(code, format!("{reason:#}"), row);
                write_line(&mut out, &serde_json::to_value(&receipt)?)?;
            }
            Err(SessionFailure::Operational(reason)) => {
                out.flush()?;
                return Err(reason.context(format!("operational failure at request {row}")));
            }
        }
    }
    out.flush()?;
    Ok(())
}

/// Write one compact response line and flush it: the caller is waiting on
/// it, so buffering would deadlock.
fn write_line(out: &mut impl Write, value: &serde_json::Value) -> anyhow::Result<()> {
    writeln!(out, "{}", serde_json::to_string(value)?).context("writing a response line")?;
    out.flush().context("flushing a response line")?;
    Ok(())
}

/// `read_line` with the runaway guard, so input with no newline cannot
/// allocate without bound. Decodes UTF-8 once per whole line, since a
/// character can straddle two buffer fills. Returns the bytes read; 0 is
/// EOF.
fn read_line_capped(input: &mut impl BufRead, line: &mut String) -> anyhow::Result<usize> {
    let mut bytes = Vec::new();
    loop {
        let chunk = input.fill_buf().context("reading a request line")?;
        if chunk.is_empty() {
            break; // EOF (possibly mid-line; the trim handles it).
        }
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (chunk.len(), false),
        };
        if bytes.len() + take > MAX_REQUEST_LINE {
            anyhow::bail!(
                "a request line exceeded {MAX_REQUEST_LINE} bytes; a half-read line \
                 cannot be resynchronised, so the session aborts"
            );
        }
        bytes.extend_from_slice(&chunk[..take]);
        input.consume(take);
        if done {
            break;
        }
    }
    line.push_str(std::str::from_utf8(&bytes).context("a request line is not valid UTF-8")?);
    Ok(bytes.len())
}

#[cfg(test)]
mod tests;

/// Decode and dispatch one request line. `op` is removed by hand, not via
/// a serde tagged enum, because those cannot `deny_unknown_fields`, and a
/// misspelt field must be refused, never read as "all predicates".
async fn handle_line(
    args: &SessionArgs,
    program: &PgProgram,
    pool: &PgPool,
    ready: &serde_json::Value,
    line: &str,
    row: u64,
) -> Result<serde_json::Value, SessionFailure> {
    let mut value: serde_json::Value = serde_json::from_str(line)
        .map_err(|e| SessionFailure::request(ErrorCode::InvalidRequest, e.into()))?;
    let Some(body) = value.as_object_mut() else {
        return Err(SessionFailure::request(
            ErrorCode::InvalidRequest,
            anyhow!("a request is a JSON object with an `op` field"),
        ));
    };
    let Some(serde_json::Value::String(op)) = body.remove("op") else {
        return Err(SessionFailure::request(
            ErrorCode::InvalidRequest,
            anyhow!("a request names its operation in a string `op` field"),
        ));
    };
    match op.as_str() {
        "propose" => handle_propose(args, program, pool, value, row).await,
        "transact" => handle_transact(args, program, pool, value, row).await,
        "claims" => handle_claims(args, program.prepared(), pool, value).await,
        "derived" => handle_derived(args, program.prepared(), pool, value).await,
        "audit" => handle_audit(args, program.prepared(), pool, value).await,
        "rejections" => handle_rejections(pool, value).await,
        "ping" => handle_ping(pool, ready, value).await,
        other => Err(SessionFailure::request(
            ErrorCode::UnknownOperation,
            anyhow!(
                "unknown operation `{other}`; this session answers propose, transact, claims, \
                 derived, audit, rejections, ping"
            ),
        )),
    }
}

/// The propose body: the batch row's own fields plus the per-request
/// explanation flag.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposeBody {
    transformation: String,
    actor: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    args_named: Option<serde_json::Value>,
    #[serde(default)]
    explain_on_reject: bool,
}

async fn handle_propose(
    args: &SessionArgs,
    program: &PgProgram,
    pool: &PgPool,
    body: serde_json::Value,
    row: u64,
) -> Result<serde_json::Value, SessionFailure> {
    let body: ProposeBody = serde_json::from_value(body)
        .map_err(|e| SessionFailure::request(ErrorCode::InvalidRequest, e.into()))?;
    let batch_row = BatchRow {
        transformation: body.transformation,
        actor: body.actor,
        args: body.args,
        args_named: body.args_named,
    };
    let mut envelope =
        propose_row_outcome(&args.file, body.explain_on_reject, program, pool, batch_row)
            .await
            .map_err(|e| match e {
                RowError { code: None, reason } => SessionFailure::Operational(reason),
                RowError {
                    code: Some(code),
                    reason,
                } => SessionFailure::request(code.into(), reason),
            })?;
    if let Some(receipt) = envelope.as_object_mut() {
        receipt.insert("row".to_string(), serde_json::json!(row));
    }
    Ok(envelope)
}

/// The transact body: the acts, each in the batch row shape, strict.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactBody {
    acts: Vec<Act>,
}

/// Several proposals as one decision, answered with the outcome plus this
/// request's row. A known error for the batch is an ordinary coded receipt.
async fn handle_transact(
    args: &SessionArgs,
    program: &PgProgram,
    pool: &PgPool,
    body: serde_json::Value,
    row: u64,
) -> Result<serde_json::Value, SessionFailure> {
    let body: TransactBody = serde_json::from_value(body)
        .map_err(|e| SessionFailure::request(ErrorCode::InvalidRequest, e.into()))?;
    let proposals = decode_acts(&args.file, program.prepared(), body.acts).map_err(row_failure)?;
    let outcome = morpholog_postgres::propose_all_against_pg(pool, program, &proposals)
        .await
        .map_err(|e| row_failure(classify_pg_error(e)))?;
    let mut envelope = serde_json::to_value(&outcome)
        .context("serialising the outcome")
        .map_err(SessionFailure::Operational)?;
    if let Some(object) = envelope.as_object_mut() {
        object.insert("row".to_string(), serde_json::json!(row));
    }
    Ok(envelope)
}

fn row_failure(e: RowError) -> SessionFailure {
    match e {
        RowError { code: None, reason } => SessionFailure::Operational(reason),
        RowError {
            code: Some(code),
            reason,
        } => SessionFailure::request(code.into(), reason),
    }
}

/// The claims read body: the generated client's `claims`/
/// `claims_named` parameters, verbatim.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimsBody {
    #[serde(default)]
    predicates: Vec<String>,
    #[serde(default)]
    named: bool,
    #[serde(default)]
    as_of: Option<String>,
    #[serde(default, rename = "where")]
    filters: Option<std::collections::BTreeMap<String, String>>,
}

async fn handle_claims(
    args: &SessionArgs,
    prepared: &PreparedProgram,
    pool: &PgPool,
    body: serde_json::Value,
) -> Result<serde_json::Value, SessionFailure> {
    let body: ClaimsBody = serde_json::from_value(body)
        .map_err(|e| SessionFailure::request(ErrorCode::InvalidRequest, e.into()))?;
    let program = prepared.program();
    // As in the one-shot named read, a predicate the programme does not
    // declare is an error.
    if body.named {
        for requested in &body.predicates {
            if !program
                .predicates
                .iter()
                .any(|d| d.name.as_str() == requested.as_str())
            {
                return Err(SessionFailure::request(
                    ErrorCode::InvalidArguments,
                    anyhow!("requested predicate `{requested}` is not declared in the programme"),
                ));
            }
        }
    }
    let (filters, declared_arity) =
        resolve_filters(&body.filters, body.named, &body.predicates, prepared)?;
    let as_of = parse_as_of(pool, &body.as_of).await?;
    let claims = claims_rows(pool, as_of, &body.predicates, &filters, declared_arity)
        .await
        .map_err(SessionFailure::Operational)?;
    if body.named {
        // A mismatch between programme and stored rows is not the
        // request's fault, so it aborts, as the one-shot read errors.
        let rows = decode_claims_named(program, &args.file, &claims)
            .map_err(SessionFailure::Operational)?;
        serde_json::to_value(rows).map_err(|e| SessionFailure::Operational(e.into()))
    } else {
        serde_json::to_value(claims).map_err(|e| SessionFailure::Operational(e.into()))
    }
}

/// The derived read body: the generated client's `derived`/
/// `derived_named` parameters, verbatim. `name` is required.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DerivedBody {
    name: String,
    #[serde(default)]
    named: bool,
    #[serde(default)]
    as_of: Option<String>,
    #[serde(default, rename = "where")]
    filters: Option<std::collections::BTreeMap<String, String>>,
}

async fn handle_derived(
    args: &SessionArgs,
    prepared: &PreparedProgram,
    pool: &PgPool,
    body: serde_json::Value,
) -> Result<serde_json::Value, SessionFailure> {
    let body: DerivedBody = serde_json::from_value(body)
        .map_err(|e| SessionFailure::request(ErrorCode::InvalidRequest, e.into()))?;
    let program = prepared.program();
    let Some(derived) = program.derived_claim(&body.name) else {
        return Err(SessionFailure::request(
            ErrorCode::InvalidArguments,
            anyhow!(
                "derived claim `{}` is not declared in the programme",
                body.name
            ),
        ));
    };
    let (filters, _) = resolve_filters(
        &body.filters,
        true,
        std::slice::from_ref(&body.name),
        prepared,
    )?;
    let as_of = parse_as_of(pool, &body.as_of).await?;
    let rows = derived_rows(
        pool,
        prepared.validated(),
        derived.predicate.as_str(),
        as_of,
        &filters,
    )
    .await
    .map_err(SessionFailure::Operational)?;
    if body.named {
        let rows =
            decode_claims_named(program, &args.file, &rows).map_err(SessionFailure::Operational)?;
        serde_json::to_value(rows).map_err(|e| SessionFailure::Operational(e.into()))
    } else {
        serde_json::to_value(rows).map_err(|e| SessionFailure::Operational(e.into()))
    }
}

/// Resolve a claims-read `where` map as the one-shot read does: it needs
/// the named read, to resolve field names, and exactly one predicate.
fn resolve_filters(
    filters: &Option<std::collections::BTreeMap<String, String>>,
    named: bool,
    predicates: &[String],
    prepared: &PreparedProgram,
) -> Result<(Vec<FieldFilter>, i32), SessionFailure> {
    let pairs: Vec<String> = filters
        .iter()
        .flatten()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    crate::commands::filter::resolve_where(named.then(|| prepared.program()), predicates, &pairs)
        .map_err(|e| SessionFailure::request(ErrorCode::InvalidArguments, e))
}

/// Parse and resolve an `as_of` coordinate. A malformed one is the
/// request's fault; failing to resolve a valid one is operational.
async fn parse_as_of(
    pool: &PgPool,
    as_of: &Option<String>,
) -> Result<Option<uuid::Uuid>, SessionFailure> {
    let Some(text) = as_of else { return Ok(None) };
    let parsed: crate::AsOf = text
        .parse()
        .map_err(|e: String| SessionFailure::request(ErrorCode::InvalidArguments, anyhow!(e)))?;
    resolve_as_of(pool, Some(parsed))
        .await
        .map_err(SessionFailure::Operational)
}

/// The ping body: nothing beyond the operation.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PingBody {}

/// The ready line again, after a round trip that also checks the
/// database still has the schema this binary was started against. It
/// says the session could reach the database just now, not that the next
/// proposal will commit: the pool reconnects on its own after a dropped
/// connection.
async fn handle_ping(
    pool: &PgPool,
    ready: &serde_json::Value,
    body: serde_json::Value,
) -> Result<serde_json::Value, SessionFailure> {
    let _: PingBody = serde_json::from_value(body)
        .map_err(|e| SessionFailure::request(ErrorCode::InvalidRequest, e.into()))?;
    morpholog_postgres::require_current_schema(pool)
        .await
        .context("checking the database")
        .map_err(SessionFailure::Operational)?;
    Ok(ready.clone())
}

/// The rejections read body: the generated client's `rejections`
/// parameters, verbatim.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RejectionsBody {
    #[serde(default = "default_rejections_limit")]
    limit: u32,
    #[serde(default)]
    mentions: Option<String>,
}

/// The one-shot `inspect rejections` default.
fn default_rejections_limit() -> u32 {
    100
}

async fn handle_rejections(
    pool: &PgPool,
    body: serde_json::Value,
) -> Result<serde_json::Value, SessionFailure> {
    let body: RejectionsBody = serde_json::from_value(body)
        .map_err(|e| SessionFailure::request(ErrorCode::InvalidRequest, e.into()))?;
    refuse_empty_mentions(body.mentions.as_deref())?;
    let rows = morpholog_postgres::list_rejection_rows(pool, body.limit, body.mentions.as_deref())
        .await
        .context("reading the rejection log")
        .map_err(SessionFailure::Operational)?;
    serde_json::to_value(rows).map_err(|e| SessionFailure::Operational(e.into()))
}

/// The audit read body: the generated client's `audit`/`audit_named`
/// parameters, plus `limit`, which only the session has: the one-shot
/// tail streams, and this answer is held whole.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditBody {
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    named: bool,
    #[serde(default)]
    mentions: Option<String>,
    #[serde(default)]
    writer_roles: Option<Vec<String>>,
    #[serde(default)]
    limit: Option<u32>,
}

/// A tail opened for this request alone, read to its horizon or to
/// `limit` rows. Stopping early loses nothing: the rows come in resume
/// order, so the next request resumes after the last one returned.
async fn handle_audit(
    args: &SessionArgs,
    prepared: &PreparedProgram,
    pool: &PgPool,
    body: serde_json::Value,
) -> Result<serde_json::Value, SessionFailure> {
    let body: AuditBody = serde_json::from_value(body)
        .map_err(|e| SessionFailure::request(ErrorCode::InvalidRequest, e.into()))?;
    refuse_empty_mentions(body.mentions.as_deref())?;
    if body.limit == Some(0) {
        return Err(invalid_arguments(anyhow!(
            "a limit of 0 reads nothing; omit it to read to the horizon"
        )));
    }
    let after = body
        .after
        .as_deref()
        .map(uuid::Uuid::parse_str)
        .transpose()
        .map_err(|e| invalid_arguments(anyhow!("`after` is not a transition id: {e}")))?;
    let limit = body
        .limit
        .map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(usize::MAX));
    let mut tail = morpholog_postgres::begin_audit_tail(pool, after, body.writer_roles.as_deref())
        .await
        .map_err(audit_failure)?;
    if let Some(subject) = &body.mentions {
        tail = tail.mentioning(subject).map_err(audit_failure)?;
    }
    let mut rows = Vec::new();
    while rows.len() < limit {
        let page = tail.next_page().await.map_err(audit_failure)?;
        if page.is_empty() {
            break;
        }
        rows.extend(page);
    }
    rows.truncate(limit);
    let program = prepared.program();
    let lines = rows
        .iter()
        .map(|row| {
            if body.named {
                audit_row_named_json(program, &args.file, row)
            } else {
                serde_json::to_value(row).map_err(Into::into)
            }
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map_err(SessionFailure::Operational)?;
    Ok(serde_json::Value::Array(lines))
}

/// What the request can correct is a receipt; a database that cannot
/// give a sound horizon aborts, so an empty answer never stands in for
/// one it could not compute.
fn audit_failure(e: PgError) -> SessionFailure {
    match e {
        PgError::TransitionNotFound(_)
        | PgError::WriterRoleUnknown { .. }
        | PgError::WriterAssertionEmpty => invalid_arguments(e.into()),
        other => {
            SessionFailure::Operational(anyhow::Error::new(other).context("reading the audit tail"))
        }
    }
}

fn refuse_empty_mentions(mentions: Option<&str>) -> Result<(), SessionFailure> {
    if mentions == Some("") {
        return Err(invalid_arguments(anyhow!(
            "`mentions` needs a subject; an empty one names nothing"
        )));
    }
    Ok(())
}

fn invalid_arguments(reason: anyhow::Error) -> SessionFailure {
    SessionFailure::request(ErrorCode::InvalidArguments, reason)
}
