//! `morpholog session` - a resident process over stdio: parse and
//! validate the programme once, hold one warm connection, then answer
//! NDJSON requests on stdin with the same pinned envelopes the
//! one-shot commands print, one compact line per request, in order.
//!
//! The protocol is deliberately lockstep: one request in, one
//! response out, no correlation ids and no interleaving. A propose
//! answers with the batch receipt shape (`row` = the 1-based request
//! line number); the reads answer with the pinned claim arrays; a
//! per-request failure answers with a session error receipt carrying
//! a stable `code`, because a caller deciding whether a retry is safe
//! must never parse prose. Operational failure (a dead connection, a
//! schema mismatch) aborts the process with a non-zero exit - to a
//! caller with a request in flight that means the outcome is UNKNOWN,
//! which the generated client surfaces as its outcome-unknown error,
//! never as a silent retry.
//!
//! The programme is pinned at start: the ready line's `model_hash` is
//! the staleness token, and editing the file never changes a running
//! session - rolling out a new model means starting new sessions.

use anyhow::{Context, anyhow};
use std::io::{BufRead, Write};

use crate::SessionArgs;
use crate::commands::filter::FieldFilter;
use crate::commands::inspect::{claims_rows, decode_claims_named, derived_rows, resolve_as_of};
use crate::commands::propose::{BatchRow, RowErrorKind, propose_row_outcome};
use crate::commands::{compile_or_exit, parse_or_exit};
use morpholog_cli::envelopes::{SessionErrorCode, SessionErrorReceipt, SessionReady};
use morpholog_core::CompiledProgram;
use morpholog_postgres::PgPool;

/// A runaway guard, not a working limit: a request line larger than
/// this aborts the session (there is no way to resynchronise a
/// half-read line, so it cannot be a receipt).
const MAX_REQUEST_LINE: usize = 64 * 1024 * 1024;

/// A per-request failure becomes a receipt and the session continues;
/// an operational failure aborts the process, because pretending the
/// stream is still healthy would make infrastructure failure look
/// like answered requests.
enum SessionFailure {
    Request {
        code: SessionErrorCode,
        reason: anyhow::Error,
    },
    Operational(anyhow::Error),
}

impl SessionFailure {
    fn request(code: SessionErrorCode, reason: anyhow::Error) -> Self {
        SessionFailure::Request { code, reason }
    }
}

pub(crate) async fn run(args: SessionArgs) -> anyhow::Result<()> {
    // Startup failures keep the one-shot exit shape: nothing has been
    // promised on stdout yet, so diagnostics + exit is the contract.
    let parsed = parse_or_exit(&args.file)?;
    let compiled = compile_or_exit(&parsed);

    // One connection: a lockstep protocol cannot use more, and the
    // cap bounds database connection load when many application
    // workers each hold a session.
    let pool = crate::commands::connect_single(&args.db.database_url).await?;

    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let ready = SessionReady::new(
        morpholog_core::format::canonical_hash(compiled.program()),
        compiled.program().name.clone(),
    );
    write_line(&mut out, &serde_json::to_value(&ready)?)?;

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
        match handle_line(&args, &compiled, &pool, line.trim(), row).await {
            Ok(response) => write_line(&mut out, &response)?,
            Err(SessionFailure::Request { code, reason }) => {
                let receipt = SessionErrorReceipt::new(code, format!("{reason:#}"), row);
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

/// One compact response line, flushed before the next request is
/// read: the caller is blocked on this line, so buffering it would
/// deadlock the conversation.
fn write_line(out: &mut impl Write, value: &serde_json::Value) -> anyhow::Result<()> {
    writeln!(out, "{}", serde_json::to_string(value)?).context("writing a response line")?;
    out.flush().context("flushing a response line")?;
    Ok(())
}

/// `read_line` with the runaway guard: accumulates through the
/// buffered reader so an input that never supplies a newline cannot
/// allocate without bound. Bytes accumulate first and decode ONCE at
/// the end of the line - a multibyte character split across two
/// buffer fills is valid UTF-8 only in whole. Returns the bytes
/// read; 0 is EOF.
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
mod tests {
    use super::read_line_capped;
    use std::io::BufReader;

    #[test]
    fn a_multibyte_character_split_across_buffer_fills_decodes_whole() {
        // A two-byte reader capacity forces the fill boundary through
        // the middle of the two-byte `é`; per-chunk decoding refused
        // this as invalid UTF-8.
        let mut input = BufReader::with_capacity(2, "h\u{e9}llo\n{\"op\":\"x\"}\n".as_bytes());
        let mut line = String::new();
        let n = read_line_capped(&mut input, &mut line).expect("valid line");
        assert_eq!(line, "h\u{e9}llo\n");
        assert_eq!(n, line.len());
        line.clear();
        read_line_capped(&mut input, &mut line).expect("next line intact");
        assert_eq!(line, "{\"op\":\"x\"}\n");
    }

    #[test]
    fn eof_mid_line_returns_what_arrived() {
        let mut input = BufReader::with_capacity(3, "tail without newline".as_bytes());
        let mut line = String::new();
        let n = read_line_capped(&mut input, &mut line).expect("reads to EOF");
        assert_eq!(line, "tail without newline");
        assert_eq!(n, line.len());
        assert_eq!(read_line_capped(&mut input, &mut line).unwrap(), 0);
    }

    #[test]
    fn genuinely_invalid_utf8_is_still_refused() {
        let mut input = BufReader::new(&b"\xff\xfe\n"[..]);
        let mut line = String::new();
        let err = read_line_capped(&mut input, &mut line).expect_err("invalid bytes");
        assert!(format!("{err:#}").contains("not valid UTF-8"));
    }
}

/// Decode and dispatch one request line. The `op` discriminator is
/// read and removed by hand rather than through an internally tagged
/// enum, because serde's tagged enums cannot enforce
/// `deny_unknown_fields` on their variants - and a misspelt field
/// must be a refusal, never silently "all predicates".
async fn handle_line(
    args: &SessionArgs,
    compiled: &CompiledProgram,
    pool: &PgPool,
    line: &str,
    row: u64,
) -> Result<serde_json::Value, SessionFailure> {
    let mut value: serde_json::Value = serde_json::from_str(line)
        .map_err(|e| SessionFailure::request(SessionErrorCode::InvalidRequest, e.into()))?;
    let Some(body) = value.as_object_mut() else {
        return Err(SessionFailure::request(
            SessionErrorCode::InvalidRequest,
            anyhow!("a request is a JSON object with an `op` field"),
        ));
    };
    let Some(serde_json::Value::String(op)) = body.remove("op") else {
        return Err(SessionFailure::request(
            SessionErrorCode::InvalidRequest,
            anyhow!("a request names its operation in a string `op` field"),
        ));
    };
    match op.as_str() {
        "propose" => handle_propose(args, compiled, pool, value, row).await,
        "claims" => handle_claims(args, compiled, pool, value).await,
        "derived" => handle_derived(args, compiled, pool, value).await,
        other => Err(SessionFailure::request(
            SessionErrorCode::UnknownOperation,
            anyhow!("unknown operation `{other}`; this session answers propose, claims, derived"),
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
    compiled: &CompiledProgram,
    pool: &PgPool,
    body: serde_json::Value,
    row: u64,
) -> Result<serde_json::Value, SessionFailure> {
    let body: ProposeBody = serde_json::from_value(body)
        .map_err(|e| SessionFailure::request(SessionErrorCode::InvalidRequest, e.into()))?;
    let batch_row = BatchRow {
        transformation: body.transformation,
        actor: body.actor,
        args: body.args,
        args_named: body.args_named,
    };
    let mut envelope = propose_row_outcome(
        &args.file,
        body.explain_on_reject,
        compiled,
        pool,
        batch_row,
    )
    .await
    .map_err(|e| match e.kind {
        RowErrorKind::Operational => SessionFailure::Operational(e.reason),
        kind => SessionFailure::request(error_code(kind), e.reason),
    })?;
    if let Some(receipt) = envelope.as_object_mut() {
        receipt.insert("row".to_string(), serde_json::json!(row));
    }
    Ok(envelope)
}

/// The stable code for a per-request propose failure. Operational is
/// unreachable here - the caller aborts on it before mapping.
fn error_code(kind: RowErrorKind) -> SessionErrorCode {
    match kind {
        RowErrorKind::MalformedRow => SessionErrorCode::InvalidRequest,
        RowErrorKind::UnknownTransformation => SessionErrorCode::UnknownTransformation,
        RowErrorKind::BadArgs => SessionErrorCode::InvalidArguments,
        RowErrorKind::Serialization => SessionErrorCode::SerializationFailure,
        RowErrorKind::Kernel => SessionErrorCode::KernelError,
        RowErrorKind::DuplicateIntent => SessionErrorCode::DuplicateIntent,
        RowErrorKind::Operational => unreachable!("operational failures abort, never map"),
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
    compiled: &CompiledProgram,
    pool: &PgPool,
    body: serde_json::Value,
) -> Result<serde_json::Value, SessionFailure> {
    let body: ClaimsBody = serde_json::from_value(body)
        .map_err(|e| SessionFailure::request(SessionErrorCode::InvalidRequest, e.into()))?;
    let program = compiled.program();
    // The named read keeps its one-shot contract: the programme is
    // the authority, and a requested predicate it does not declare is
    // the typo this mode exists to catch.
    if body.named {
        for requested in &body.predicates {
            if !program
                .predicates
                .iter()
                .any(|d| d.name.as_str() == requested.as_str())
            {
                return Err(SessionFailure::request(
                    SessionErrorCode::InvalidArguments,
                    anyhow!("requested predicate `{requested}` is not declared in the programme"),
                ));
            }
        }
    }
    let (filters, declared_arity) =
        resolve_filters(&body.filters, body.named, &body.predicates, compiled)?;
    let as_of = parse_as_of(pool, &body.as_of).await?;
    let claims = claims_rows(pool, as_of, &body.predicates, &filters, declared_arity)
        .await
        .map_err(SessionFailure::Operational)?;
    if body.named {
        // Skew between the programme and the stored rows is not the
        // request's fault: it aborts, exactly as the one-shot read
        // errors rather than answering.
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
    compiled: &CompiledProgram,
    pool: &PgPool,
    body: serde_json::Value,
) -> Result<serde_json::Value, SessionFailure> {
    let body: DerivedBody = serde_json::from_value(body)
        .map_err(|e| SessionFailure::request(SessionErrorCode::InvalidRequest, e.into()))?;
    let program = compiled.program();
    let Some(derived) = program.derived_claim(&body.name) else {
        return Err(SessionFailure::request(
            SessionErrorCode::InvalidArguments,
            anyhow!(
                "derived claim `{}` is not declared in the programme",
                body.name
            ),
        ));
    };
    let filters = match &body.filters {
        None => Vec::new(),
        Some(map) if map.is_empty() => Vec::new(),
        Some(map) => {
            let Some(decl) = program.predicate(&body.name) else {
                return Err(SessionFailure::request(
                    SessionErrorCode::InvalidArguments,
                    anyhow!(
                        "`where` needs `{}` declared as a predicate to resolve field names",
                        body.name
                    ),
                ));
            };
            let pairs: Vec<String> = map.iter().map(|(k, v)| format!("{k}={v}")).collect();
            crate::commands::filter::resolve(decl, &pairs)
                .map_err(|e| SessionFailure::request(SessionErrorCode::InvalidArguments, e))?
        }
    };
    let as_of = parse_as_of(pool, &body.as_of).await?;
    let rows = derived_rows(pool, &program.definitions, derived, as_of, &filters)
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

/// Resolve a claims-read `where` map under the one-shot contract: it
/// needs the named read (field names resolve against a declaration)
/// and exactly one predicate (the fields belong to one claim shape).
fn resolve_filters(
    filters: &Option<std::collections::BTreeMap<String, String>>,
    named: bool,
    predicates: &[String],
    compiled: &CompiledProgram,
) -> Result<(Vec<FieldFilter>, i32), SessionFailure> {
    let map = match filters {
        None => return Ok((Vec::new(), 0)),
        Some(map) if map.is_empty() => return Ok((Vec::new(), 0)),
        Some(map) => map,
    };
    if !named {
        return Err(SessionFailure::request(
            SessionErrorCode::InvalidArguments,
            anyhow!("`where` needs the named read: field names resolve against a declaration"),
        ));
    }
    let [predicate] = predicates else {
        return Err(SessionFailure::request(
            SessionErrorCode::InvalidArguments,
            anyhow!(
                "`where` needs exactly one predicate, because the field names belong \
                 to one claim shape; got {}",
                predicates.len()
            ),
        ));
    };
    let Some(decl) = compiled.program().predicate(predicate) else {
        return Err(SessionFailure::request(
            SessionErrorCode::InvalidArguments,
            anyhow!("predicate `{predicate}` is not declared in the programme"),
        ));
    };
    let declared_arity = i32::try_from(decl.args.len()).map_err(|_| {
        SessionFailure::request(
            SessionErrorCode::InvalidArguments,
            anyhow!("`{predicate}` declares too many arguments to filter"),
        )
    })?;
    let pairs: Vec<String> = map.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let filters = crate::commands::filter::resolve(decl, &pairs)
        .map_err(|e| SessionFailure::request(SessionErrorCode::InvalidArguments, e))?;
    Ok((filters, declared_arity))
}

/// Parse and resolve an `as_of` coordinate. A malformed coordinate is
/// the request's fault; resolving a well-formed one touches the
/// database, where failure is operational.
async fn parse_as_of(
    pool: &PgPool,
    as_of: &Option<String>,
) -> Result<Option<uuid::Uuid>, SessionFailure> {
    let Some(text) = as_of else { return Ok(None) };
    let parsed: crate::AsOf = text.parse().map_err(|e: String| {
        SessionFailure::request(SessionErrorCode::InvalidArguments, anyhow!(e))
    })?;
    resolve_as_of(pool, Some(parsed))
        .await
        .map_err(SessionFailure::Operational)
}
