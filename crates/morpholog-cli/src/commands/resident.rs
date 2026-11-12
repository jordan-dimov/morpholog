//! The resident runtime: one programme fixed at start, one warm
//! connection, and one request-to-envelope dispatch with no transport of
//! its own. `session` wraps it in a stdio loop and answers every
//! operation; `serve` wraps it in a socket, one connection per presenter,
//! and answers only what each presenter is granted.
//!
//! The programme is fixed when the runtime starts. With an expected hash,
//! a programme whose canonical hash is any other is refused before the
//! runtime connects to anything, so a deployment that starts the process
//! with the hash it authorised cannot be made to run other rules by an
//! edited file. Editing the file never changes a running runtime, so a
//! new model means a new process.
//!
//! A per-request failure is a receipt and the runtime continues. An
//! operational failure is the front-end's to report as an abort, so it
//! never looks like an answered request.

use anyhow::{Context, anyhow};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::commands::filter::FieldFilter;
use crate::commands::inspect::{
    audit_row_named_json, claims_rows, decode_claim_reads_named, decode_claims_named, derived_rows,
    resolve_as_of,
};
use crate::commands::propose::{
    BatchRow, RowError, classify_pg_error, decode_row, propose_row_outcome,
};
use crate::commands::transact::{Act, decode_acts};
use crate::commands::{parse_or_report, prepare_or_report};
use morpholog_cli::envelopes::{ErrorCode, SessionReady};
use morpholog_core::PreparedProgram;
use morpholog_postgres::{PgError, PgPool, PgProgram};

pub(crate) enum ResidentFailure {
    Request {
        code: ErrorCode,
        reason: anyhow::Error,
    },
    Operational(anyhow::Error),
}

impl ResidentFailure {
    fn request(code: ErrorCode, reason: anyhow::Error) -> Self {
        ResidentFailure::Request { code, reason }
    }
}

/// The operations that need a grant. Proposing (`propose`, `transact`)
/// and `ping` need none: the rules decide a proposal. A read or an
/// explanation tells the caller about state it may have no business
/// seeing, and repeated explanations can map out a path the rules allow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Gated {
    Explain,
    Claims,
    Derived,
    Audit,
    Rejections,
}

impl Gated {
    const ALL: [Gated; 5] = [
        Gated::Explain,
        Gated::Claims,
        Gated::Derived,
        Gated::Audit,
        Gated::Rejections,
    ];

    fn name(self) -> &'static str {
        match self {
            Gated::Explain => "explain",
            Gated::Claims => "claims",
            Gated::Derived => "derived",
            Gated::Audit => "audit",
            Gated::Rejections => "rejections",
        }
    }

    pub(crate) fn parse(op: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|g| g.name() == op)
    }
}

/// What one caller may ask beyond proposing.
#[derive(Clone, Debug, Default)]
pub(crate) struct Permits(Vec<Gated>);

impl Permits {
    pub(crate) fn all() -> Self {
        Self(Gated::ALL.to_vec())
    }

    pub(crate) fn grant(&mut self, gated: Gated) {
        if !self.allows(gated) {
            self.0.push(gated);
        }
    }

    fn allows(&self, gated: Gated) -> bool {
        self.0.contains(&gated)
    }

    fn refuse(op: &str) -> ResidentFailure {
        ResidentFailure::request(
            ErrorCode::UnknownOperation,
            anyhow!("`{op}` is not granted to this caller; it may propose, transact and ping"),
        )
    }
}

/// The programme a runtime serves, parsed, validated and pinned once,
/// and shared by every connection that serves it.
pub(crate) struct Programme {
    file: PathBuf,
    program: PgProgram,
    ready: serde_json::Value,
}

impl Programme {
    /// Parse, validate and pin. Failures behave as in one-shot commands:
    /// diagnostics, then an error. A hash mismatch is refused here, before
    /// anything connects.
    pub(crate) fn load(file: &Path, expect_hash: Option<&str>) -> anyhow::Result<Self> {
        let parsed = parse_or_report(file)?;
        let program = PgProgram::new(prepare_or_report(&parsed)?);
        let model_hash = morpholog_core::format::canonical_hash(program.prepared().program());
        if let Some(expected) = expect_hash
            && expected != model_hash
        {
            anyhow::bail!(
                "the programme in `{}` hashes to {model_hash}, not the {expected} this runtime \
                 was started to serve; nothing was started",
                file.display()
            );
        }
        let ready = SessionReady::new(model_hash, program.prepared().program().name.clone());
        let ready = serde_json::to_value(&ready)?;
        Ok(Self {
            file: file.to_path_buf(),
            program,
            ready,
        })
    }

    /// The ready line: the identity this programme is served under.
    pub(crate) fn ready(&self) -> &serde_json::Value {
        &self.ready
    }
}

pub(crate) struct Resident {
    programme: Arc<Programme>,
    pool: PgPool,
}

impl Resident {
    /// Load the programme, then connect.
    pub(crate) async fn start(
        file: &Path,
        database_url: &str,
        expect_hash: Option<&str>,
    ) -> anyhow::Result<Self> {
        let programme = Arc::new(Programme::load(file, expect_hash)?);
        let pool = crate::commands::connect_single(database_url).await?;
        Ok(Self::new(programme, pool))
    }

    pub(crate) fn new(programme: Arc<Programme>, pool: PgPool) -> Self {
        Self { programme, pool }
    }

    /// The ready line: the identity this runtime serves.
    pub(crate) fn ready(&self) -> &serde_json::Value {
        &self.programme.ready
    }

    fn prepared(&self) -> &PreparedProgram {
        self.programme.program.prepared()
    }

    /// Decode and dispatch one request. `op` is removed by hand, not via
    /// a serde tagged enum, because those cannot `deny_unknown_fields`,
    /// and a misspelt field must be refused, never read as "all
    /// predicates". `row` is the request's 1-based number, carried on
    /// the receipts that have one.
    pub(crate) async fn handle(
        &self,
        line: &str,
        row: u64,
        permits: &Permits,
    ) -> Result<serde_json::Value, ResidentFailure> {
        let mut value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| ResidentFailure::request(ErrorCode::InvalidRequest, e.into()))?;
        let Some(body) = value.as_object_mut() else {
            return Err(ResidentFailure::request(
                ErrorCode::InvalidRequest,
                anyhow!("a request is a JSON object with an `op` field"),
            ));
        };
        let Some(serde_json::Value::String(op)) = body.remove("op") else {
            return Err(ResidentFailure::request(
                ErrorCode::InvalidRequest,
                anyhow!("a request names its operation in a string `op` field"),
            ));
        };
        if let Some(gated) = Gated::parse(&op)
            && !permits.allows(gated)
        {
            return Err(Permits::refuse(&op));
        }
        match op.as_str() {
            "propose" => self.propose(value, row, permits).await,
            "explain" => self.explain(value, row).await,
            "transact" => self.transact(value, row).await,
            "claims" => self.claims(value).await,
            "derived" => self.derived(value).await,
            "audit" => self.audit(value).await,
            "rejections" => self.rejections(value).await,
            "ping" => self.ping(value).await,
            other => Err(ResidentFailure::request(
                ErrorCode::UnknownOperation,
                anyhow!(
                    "unknown operation `{other}`; this session answers propose, explain, \
                     transact, claims, derived, audit, rejections, ping"
                ),
            )),
        }
    }

    async fn propose(
        &self,
        body: serde_json::Value,
        row: u64,
        permits: &Permits,
    ) -> Result<serde_json::Value, ResidentFailure> {
        let body: ProposeBody = serde_json::from_value(body)
            .map_err(|e| ResidentFailure::request(ErrorCode::InvalidRequest, e.into()))?;
        if body.explain_on_reject && !permits.allows(Gated::Explain) {
            return Err(ResidentFailure::request(
                ErrorCode::InvalidRequest,
                anyhow!("`explain_on_reject` needs the `explain` grant, which this caller lacks"),
            ));
        }
        let batch_row = BatchRow {
            transformation: body.transformation,
            actor: body.actor,
            args: body.args,
            args_named: body.args_named,
        };
        let mut envelope = propose_row_outcome(
            body.explain_on_reject,
            &self.programme.program,
            &self.pool,
            batch_row,
        )
        .await
        .map_err(row_failure)?;
        if let Some(receipt) = envelope.as_object_mut() {
            receipt.insert("row".to_string(), serde_json::json!(row));
        }
        Ok(envelope)
    }

    /// The one-shot `explain`, answered with its explanation plus this
    /// request's row: the same decoder as a proposal, the same
    /// one-statement read of the state, the kernel in memory, nothing
    /// recorded. A kernel error is a verdict inside the explanation, as
    /// on the one-shot command; only a failure to read the state aborts.
    async fn explain(
        &self,
        body: serde_json::Value,
        row: u64,
    ) -> Result<serde_json::Value, ResidentFailure> {
        let body: ExplainBody = serde_json::from_value(body)
            .map_err(|e| ResidentFailure::request(ErrorCode::InvalidRequest, e.into()))?;
        let batch_row = BatchRow {
            transformation: body.transformation,
            actor: body.actor,
            args: body.args,
            args_named: body.args_named,
        };
        let prepared = self.prepared();
        let transition = decode_row(prepared, batch_row).map_err(row_failure)?;
        let state = morpholog_postgres::load_scoped_state(
            &self.pool,
            prepared,
            &transition.transformation_name,
        )
        .await
        .context("loading the state to explain against")
        .map_err(ResidentFailure::Operational)?
        .context("the transformation was looked up in this programme")
        .map_err(ResidentFailure::Operational)?;
        let explanation = prepared.explain(
            &transition,
            &state,
            &mut morpholog_postgres::runtime_subjects(),
        );
        let mut envelope = serde_json::to_value(&explanation)
            .context("serialising the explanation")
            .map_err(ResidentFailure::Operational)?;
        if let Some(object) = envelope.as_object_mut() {
            object.insert("row".to_string(), serde_json::json!(row));
        }
        Ok(envelope)
    }

    /// Several proposals as one decision, answered with the outcome plus
    /// this request's row. A known error for the batch is an ordinary
    /// coded receipt.
    async fn transact(
        &self,
        body: serde_json::Value,
        row: u64,
    ) -> Result<serde_json::Value, ResidentFailure> {
        let body: TransactBody = serde_json::from_value(body)
            .map_err(|e| ResidentFailure::request(ErrorCode::InvalidRequest, e.into()))?;
        let proposals = decode_acts(self.prepared(), body.acts).map_err(row_failure)?;
        let outcome = morpholog_postgres::propose_all_against_pg(
            &self.pool,
            &self.programme.program,
            &proposals,
        )
        .await
        .map_err(|e| row_failure(classify_pg_error(e)))?;
        let mut envelope = serde_json::to_value(&outcome)
            .context("serialising the outcome")
            .map_err(ResidentFailure::Operational)?;
        if let Some(object) = envelope.as_object_mut() {
            object.insert("row".to_string(), serde_json::json!(row));
        }
        Ok(envelope)
    }

    async fn claims(&self, body: serde_json::Value) -> Result<serde_json::Value, ResidentFailure> {
        let body: ClaimsBody = serde_json::from_value(body)
            .map_err(|e| ResidentFailure::request(ErrorCode::InvalidRequest, e.into()))?;
        let prepared = self.prepared();
        let program = prepared.program();
        // As in the one-shot named read, a predicate the programme does
        // not declare is an error.
        if body.named {
            for requested in &body.predicates {
                if !program
                    .predicates
                    .iter()
                    .any(|d| d.name.as_str() == requested.as_str())
                {
                    return Err(ResidentFailure::request(
                        ErrorCode::InvalidArguments,
                        anyhow!(
                            "requested predicate `{requested}` is not declared in the programme"
                        ),
                    ));
                }
            }
        }
        let (filters, declared_arity) =
            resolve_filters(&body.filters, body.named, &body.predicates, prepared)?;
        let as_of = parse_as_of(&self.pool, &body.as_of).await?;
        let claims = claims_rows(
            &self.pool,
            as_of,
            &body.predicates,
            &filters,
            declared_arity,
        )
        .await
        .map_err(ResidentFailure::Operational)?;
        if body.named {
            // A mismatch between programme and stored rows is not the
            // request's fault, so it aborts, as the one-shot read errors.
            let rows = decode_claim_reads_named(program, &self.programme.file, &claims)
                .map_err(ResidentFailure::Operational)?;
            serde_json::to_value(rows).map_err(|e| ResidentFailure::Operational(e.into()))
        } else {
            serde_json::to_value(claims).map_err(|e| ResidentFailure::Operational(e.into()))
        }
    }

    async fn derived(&self, body: serde_json::Value) -> Result<serde_json::Value, ResidentFailure> {
        let body: DerivedBody = serde_json::from_value(body)
            .map_err(|e| ResidentFailure::request(ErrorCode::InvalidRequest, e.into()))?;
        let prepared = self.prepared();
        let program = prepared.program();
        let Some(derived) = program.derived_claim(&body.name) else {
            return Err(ResidentFailure::request(
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
        let as_of = parse_as_of(&self.pool, &body.as_of).await?;
        let rows = derived_rows(
            &self.pool,
            prepared.validated(),
            derived.predicate.as_str(),
            as_of,
            &filters,
        )
        .await
        .map_err(ResidentFailure::Operational)?;
        if body.named {
            let rows = decode_claims_named(program, &self.programme.file, &rows)
                .map_err(ResidentFailure::Operational)?;
            serde_json::to_value(rows).map_err(|e| ResidentFailure::Operational(e.into()))
        } else {
            serde_json::to_value(rows).map_err(|e| ResidentFailure::Operational(e.into()))
        }
    }

    /// The ready line again, after reading the database's recorded
    /// migrations and checking they still suit this binary. It says the
    /// runtime could reach the database just now, not that the next
    /// proposal will commit: the pool reconnects on its own after a
    /// dropped connection.
    async fn ping(&self, body: serde_json::Value) -> Result<serde_json::Value, ResidentFailure> {
        let _: PingBody = serde_json::from_value(body)
            .map_err(|e| ResidentFailure::request(ErrorCode::InvalidRequest, e.into()))?;
        morpholog_postgres::require_current_schema(&self.pool)
            .await
            .context("checking the database")
            .map_err(ResidentFailure::Operational)?;
        Ok(self.programme.ready.clone())
    }

    async fn rejections(
        &self,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, ResidentFailure> {
        let body: RejectionsBody = serde_json::from_value(body)
            .map_err(|e| ResidentFailure::request(ErrorCode::InvalidRequest, e.into()))?;
        refuse_empty_mentions(body.mentions.as_deref())?;
        let rows = morpholog_postgres::list_rejection_rows(
            &self.pool,
            body.limit,
            body.mentions.as_deref(),
        )
        .await
        .context("reading the rejection log")
        .map_err(ResidentFailure::Operational)?;
        serde_json::to_value(rows).map_err(|e| ResidentFailure::Operational(e.into()))
    }

    /// A tail opened for this request alone, read to its horizon or to
    /// `limit` rows. Stopping early loses nothing: the rows come in
    /// resume order, so the next request resumes after the last one
    /// returned.
    async fn audit(&self, body: serde_json::Value) -> Result<serde_json::Value, ResidentFailure> {
        let body: AuditBody = serde_json::from_value(body)
            .map_err(|e| ResidentFailure::request(ErrorCode::InvalidRequest, e.into()))?;
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
        let mut tail = morpholog_postgres::begin_audit_tail(&self.pool, after)
            .await
            .map_err(audit_failure)?;
        if let Some(subject) = &body.mentions {
            tail = tail.mentioning(subject).map_err(audit_failure)?;
        }
        let mut rows = Vec::new();
        while rows.len() < limit {
            let page = tail
                .next_page_up_to(limit - rows.len())
                .await
                .map_err(audit_failure)?;
            if page.is_empty() {
                break;
            }
            rows.extend(page);
        }
        let program = self.prepared().program();
        let lines = rows
            .iter()
            .map(|row| {
                if body.named {
                    audit_row_named_json(program, &self.programme.file, row)
                } else {
                    serde_json::to_value(row).map_err(Into::into)
                }
            })
            .collect::<anyhow::Result<Vec<_>>>()
            .map_err(ResidentFailure::Operational)?;
        Ok(serde_json::Value::Array(lines))
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

/// The explain body: the batch row's own fields, nothing more.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExplainBody {
    transformation: String,
    actor: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    args_named: Option<serde_json::Value>,
}

/// The transact body: the acts, each in the batch row shape, strict.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactBody {
    acts: Vec<Act>,
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

/// The ping body: nothing beyond the operation.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PingBody {}

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

/// The audit read body: the generated client's `audit`/`audit_named`
/// parameters, plus `limit`, which only the resident read has: the
/// one-shot tail streams, and this answer is held whole.
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
    limit: Option<u32>,
}

fn row_failure(e: RowError) -> ResidentFailure {
    match e {
        RowError { code: None, reason } => ResidentFailure::Operational(reason),
        RowError {
            code: Some(code),
            reason,
        } => ResidentFailure::request(code.into(), reason),
    }
}

/// Resolve a claims-read `where` map as the one-shot read does: it needs
/// the named read, to resolve field names, and exactly one predicate.
fn resolve_filters(
    filters: &Option<std::collections::BTreeMap<String, String>>,
    named: bool,
    predicates: &[String],
    prepared: &PreparedProgram,
) -> Result<(Vec<FieldFilter>, i32), ResidentFailure> {
    let pairs: Vec<String> = filters
        .iter()
        .flatten()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    crate::commands::filter::resolve_where(named.then(|| prepared.program()), predicates, &pairs)
        .map_err(|e| ResidentFailure::request(ErrorCode::InvalidArguments, e))
}

/// Parse and resolve an `as_of` coordinate. A malformed one is the
/// request's fault; failing to resolve a valid one is operational.
async fn parse_as_of(
    pool: &PgPool,
    as_of: &Option<String>,
) -> Result<Option<uuid::Uuid>, ResidentFailure> {
    let Some(text) = as_of else { return Ok(None) };
    let parsed: crate::AsOf = text
        .parse()
        .map_err(|e: String| ResidentFailure::request(ErrorCode::InvalidArguments, anyhow!(e)))?;
    resolve_as_of(pool, Some(parsed))
        .await
        .map_err(ResidentFailure::Operational)
}

/// What the request can correct is a receipt; a database that cannot
/// give a sound horizon aborts, so an empty answer never stands in for
/// one it could not compute.
fn audit_failure(e: PgError) -> ResidentFailure {
    match e {
        PgError::TransitionNotFound(_) | PgError::TransitionNotSettled(_) => {
            invalid_arguments(e.into())
        }
        other => ResidentFailure::Operational(
            anyhow::Error::new(other).context("reading the audit tail"),
        ),
    }
}

fn refuse_empty_mentions(mentions: Option<&str>) -> Result<(), ResidentFailure> {
    if mentions == Some("") {
        return Err(invalid_arguments(anyhow!(
            "`mentions` needs a subject; an empty one names nothing"
        )));
    }
    Ok(())
}

fn invalid_arguments(reason: anyhow::Error) -> ResidentFailure {
    ResidentFailure::request(ErrorCode::InvalidArguments, reason)
}
