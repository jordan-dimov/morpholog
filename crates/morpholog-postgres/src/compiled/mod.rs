//! Compile fragment invariants into SQL violation queries.
//!
//! Each invariant compiles to one query that returns a witnessing row when
//! the invariant FAILS and no rows when it holds. It reproduces the kernel:
//! an invariant holds iff its body yields at least one binding;
//! `Implies`/`Forall`/`Exists`/`Not` export no bindings; `And` threads
//! bindings left to right; decimals compare scale-insensitively
//! (`::numeric`, never JSON text).
//!
//! An invariant the compiler cannot express is a [`CompileRefusal`], and
//! all of them are collected. Input is a [`ValidatedProgram`], so a refusal
//! always means a valid invariant outside the fragment, never a validation
//! error.
//!
//! Each declared kind has one equality representation ([`Equality`]),
//! with its reason, and each equality has one seekable expression
//! ([`Representation`]) an index can be built over:
//!
//! - `Decimal`: `(arguments -> N ->> 'value')::numeric`. The kernel compares
//!   decimals scale-insensitively while the stored string preserves scale
//!   (`1.0` vs `1.00`), so text or jsonb equality would be WRONG.
//! - `Subject`: `arguments -> N ->> 'value'` text. Subjects are opaque
//!   strings; kernel equality is string equality.
//! - `Bool`, `Date`, `Timestamp`, `Duration`: the whole tagged value,
//!   `arguments -> N`, compared as jsonb. Sound because these kinds
//!   deserialise into semantic types (`bool`, jiff's `Date`/`Timestamp`/
//!   `SignedDuration`) whose serde output is canonical - two kernel-equal
//!   values re-serialise to the same tagged JSON - and every stored claim
//!   passed through that serialisation.
//! - `Quantity`: the amount as `numeric` (scale-insensitive, like a
//!   decimal) and the unit as text; both must agree. The seek is the
//!   amount alone, since a record has no btree ordering, and the unit
//!   stays a residual condition.
//! - `Collection` (may hold decimals), `Any` (may hold anything): no
//!   equality representation is proved, so a variable join or filter on
//!   such a position refuses by kind.
//!
//! Ordered comparisons run over numbers: a decimal or a quantity's amount
//! as `numeric`, a timestamp as `morpholog.timestamp_nanos` (nanoseconds
//! since the epoch, the kernel's own precision). Where the kernel can
//! raise while comparing - two quantities of different units, or a stored
//! value that is not of the declared kind, which history admitted under an
//! older declaration can hold - the SQL carries that as data, the way a
//! sum's range test does: the violation query returns such a row, and a
//! second query then names the first erroring binding in the kernel's own
//! order (state as loaded, the transition's admissions at the tail) with
//! its operands, so the runner reports the kernel's exact error. An
//! error-bearing comparison must therefore close its scope, as a sum
//! comparison must.
//!
//! Witness contract: rule name, version and the witness VARIABLE SET must
//! match the kernel. Witness values may differ: a symmetric self-join can
//! name the violating pair in either order.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use morpholog_core::{
    ClaimInstance, CompareOp, EvalError, EvalValue, Impact, ImpactPlan, Invariant, InvariantName,
    OrderedDomain, PredicateArgKind, PredicateDecl, PredicateName, Prop, RejectionReason, SumSeed,
    Term, ValidatedProgram, Value, ValueExpr, Var, WitnessBinding, literal_value,
    ordered_compare_error,
};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::error::{PgError, classify};
use crate::sql_quote::{quote_ident, quote_literal};

/// Why one invariant is outside the compiled fragment. Typed so tests and
/// callers dispatch on the variant, never on message text.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompileReason {
    /// A construct the fragment has no rendering for (`or`, `pre`, a
    /// defined call, arithmetic, ...).
    Construct { construct: &'static str },
    /// An ordered comparison in a non-decimal domain.
    ComparisonDomain { domain: OrderedDomain },
    /// A variable join, filter, or extraction on a position whose declared
    /// kind has no proved equality representation.
    ArgumentKind { kind: PredicateArgKind },
    /// A literal kind the fragment cannot render as a SQL constant.
    Literal { kind: &'static str },
    /// A `sum` outside the compiled shape (target must be a bound decimal
    /// variable or a decimal literal, body must bind claims, seed decimal).
    SumShape { detail: &'static str },
    /// A quantity or timestamp ordering outside the compiled shape: it can
    /// raise, so it must close its scope, like a sum comparison.
    ComparisonShape { detail: &'static str },
    /// A shape a validated programme cannot exhibit (defensive: reachable
    /// only through IR that skipped `Program::validated`).
    UnvalidatedShape { detail: String },
}

impl std::fmt::Display for CompileReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileReason::Construct { construct } => {
                write!(f, "`{construct}` is outside the compiled fragment")
            }
            CompileReason::ComparisonDomain { domain } => {
                write!(
                    f,
                    "comparison domain {domain:?} is outside the compiled fragment"
                )
            }
            CompileReason::ArgumentKind { kind } => write!(
                f,
                "no proved equality representation for kind {kind} in the compiled fragment"
            ),
            CompileReason::Literal { kind } => {
                write!(f, "{kind} literals are outside the compiled fragment")
            }
            CompileReason::SumShape { detail } => {
                write!(f, "sum outside the compiled fragment: {detail}")
            }
            CompileReason::ComparisonShape { detail } => {
                write!(f, "comparison outside the compiled fragment: {detail}")
            }
            CompileReason::UnvalidatedShape { detail } => {
                write!(f, "shape a validated programme cannot exhibit: {detail}")
            }
        }
    }
}

/// One invariant the compiler could not express in the fragment.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CompileRefusal {
    pub invariant: InvariantName,
    pub reason: CompileReason,
}

/// Every invariant of a programme, compiled, in programme order: the
/// runner refuses on the first violation, as the kernel does.
#[derive(Debug)]
pub(crate) struct CompiledInvariantSet {
    pub(crate) invariants: Vec<CompiledInvariant>,
}

impl CompiledInvariantSet {
    /// Every index the set's SQL can seek on, once each, in
    /// specification order.
    pub(crate) fn required_indexes(&self) -> Vec<IndexSpec> {
        let mut specs: Vec<IndexSpec> = self
            .invariants
            .iter()
            .flat_map(|inv| inv.required_indexes.iter().cloned())
            .collect();
        specs.sort();
        specs.dedup();
        specs
    }
}

/// Which check runs: the whole-state check, or the check bounded to the
/// cases the effective delta could have changed, which production runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// Used by the differential: on governed history it must agree with
    /// `CaseBound`; on dirty history it is the whole-state question
    /// `evaluate` asks.
    #[cfg_attr(not(test), allow(dead_code))]
    Full,
    CaseBound,
}

/// A violation the runner found: the rule, its version, its witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SqlViolation {
    pub(crate) name: InvariantName,
    pub(crate) version: u32,
    pub(crate) witness: Vec<WitnessBinding>,
}

impl From<SqlViolation> for RejectionReason {
    fn from(v: SqlViolation) -> Self {
        RejectionReason::Invariant {
            name: v.name,
            version: v.version,
            witness: v.witness,
        }
    }
}

/// Turn off JIT for the rest of this transaction. Correlated-subquery
/// estimates push planned cost past the JIT threshold, costing ~118ms of
/// compilation for a sub-millisecond plan (measured at 100k claims).
pub(crate) async fn disable_jit(tx: &mut Transaction<'_, Postgres>) -> Result<(), PgError> {
    sqlx::raw_sql("SET LOCAL jit = off")
        .execute(&mut **tx)
        .await
        .map_err(classify)?;
    Ok(())
}

impl CompiledInvariantSet {
    /// The first violating invariant in programme order with its witness,
    /// or `None` when every check holds. Runs inside the caller's
    /// transaction, over the claims table after the delta was written. The
    /// commit path and the differential both use it, so the differential
    /// tests the code that admits transitions.
    ///
    /// Where the kernel could raise instead of deciding (a sum past the
    /// decimal range, quantities of two units), the violation query returns
    /// the row and a second query names the first erroring binding in the
    /// kernel's order: state as loaded, then each transition's admissions
    /// in statement order, which `steps` supplies. That error wins over any
    /// violation, as it does in the kernel.
    pub(crate) async fn first_violation(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        stage: Stage,
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
        steps: &[DeltaStep],
    ) -> Result<Option<SqlViolation>, PgError> {
        for inv in &self.invariants {
            let case_filter = match stage {
                Stage::Full => None,
                Stage::CaseBound => match inv.case_filter(asserted, retracted) {
                    CaseFilter::Untouched => continue,
                    CaseFilter::Bounded(filter) => Some(filter),
                    CaseFilter::Unbounded => None,
                },
            };
            let sql = inv.violation_sql(case_filter.as_deref());
            // Safe: this module renders the SQL from a validated programme,
            // with identifiers quoted, literals escaped and the name comment
            // neutralised.
            let row = sqlx::query(sqlx::AssertSqlSafe(sql))
                .fetch_optional(&mut **tx)
                .await
                .map_err(classify)?;
            let Some(row) = row else {
                continue;
            };
            if let Some(error_sql) = inv.error_sql(case_filter.as_deref(), steps)? {
                let erroring = sqlx::query(sqlx::AssertSqlSafe(error_sql))
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(classify)?;
                if let Some(erroring) = erroring {
                    return Err(PgError::Kernel(decode_error(&erroring)?));
                }
            }
            return Ok(Some(SqlViolation {
                name: inv.name.clone(),
                version: inv.version,
                witness: decode_witness(inv, &row)?,
            }));
        }
        Ok(None)
    }
}

/// One transition's admissions, in statement order. The error query
/// orders a transition's rows by it, after every row loaded before the
/// transition, as the kernel's candidate state does.
#[derive(Debug, Clone)]
pub(crate) struct DeltaStep {
    pub(crate) transition_id: Uuid,
    pub(crate) asserted: Vec<ClaimInstance>,
}

/// The ordering keys for one claim alias: which step admitted the row
/// (loaded rows first), its place in that step, then the loaded order.
fn order_keys(alias: &str, steps: &[DeltaStep]) -> Result<String, PgError> {
    if steps.is_empty() {
        return Ok(format!("{alias}.arguments_hash"));
    }
    let mut step_arms = String::new();
    let mut place_arms = String::new();
    for (i, step) in steps.iter().enumerate() {
        let id = quote_literal(&step.transition_id.to_string());
        let _ = write!(step_arms, " WHEN {id} THEN {}", i + 1);
        if step.asserted.is_empty() {
            continue;
        }
        let admitted = step
            .asserted
            .iter()
            .map(|c| {
                Ok(format!(
                    "{}::jsonb",
                    quote_literal(&serde_json::to_string(&c.args)?)
                ))
            })
            .collect::<Result<Vec<_>, PgError>>()?
            .join(", ");
        let _ = write!(
            place_arms,
            " WHEN {id} THEN COALESCE(array_position(ARRAY[{admitted}], {alias}.arguments), 0)"
        );
    }
    Ok(format!(
        "CASE {alias}.asserted_in{step_arms} ELSE 0 END, CASE {alias}.asserted_in{place_arms} ELSE 0 END, {alias}.arguments_hash"
    ))
}

/// Decode a violation row's witness columns. Each is the `::text` of the
/// full tagged value, so `EvalValue`'s serde decodes it with no per-kind
/// logic.
fn decode_witness(
    inv: &CompiledInvariant,
    row: &sqlx::postgres::PgRow,
) -> Result<Vec<WitnessBinding>, PgError> {
    let mut witness = Vec::with_capacity(inv.witness_vars.len());
    for var in &inv.witness_vars {
        let col = format!("w_{var}");
        let text: String = row
            .try_get(col.as_str())
            .map_err(|e| PgError::InvalidState(format!("witness column {col} missing: {e}")))?;
        let value: EvalValue = serde_json::from_str(&text)
            .map_err(|e| PgError::InvalidState(format!("witness value {col} undecodable: {e}")))?;
        witness.push(WitnessBinding {
            var: var.clone(),
            value,
        });
    }
    Ok(witness)
}

/// The kernel's error for the first erroring binding: a sum's range, or
/// the kernel's own verdict on the two operands it could not order.
fn decode_error(row: &sqlx::postgres::PgRow) -> Result<EvalError, PgError> {
    let column = |name: &str| -> Result<String, PgError> {
        row.try_get::<Option<String>, _>(name)
            .map_err(|e| PgError::InvalidState(format!("error column {name} missing: {e}")))?
            .ok_or_else(|| PgError::InvalidState(format!("error column {name} is null")))
    };
    match column("kind")?.as_str() {
        "range" => Ok(EvalError::sum_out_of_decimal_range()),
        "compare" => {
            let domain = match column("domain")?.as_str() {
                "decimal" => OrderedDomain::Decimal,
                "timestamp" => OrderedDomain::Timestamp,
                other => {
                    return Err(PgError::InvalidState(format!(
                        "error domain {other} is not one the compiler renders"
                    )));
                }
            };
            let operand = |name: &str| -> Result<EvalValue, PgError> {
                serde_json::from_str(&column(name)?).map_err(|e| {
                    PgError::InvalidState(format!("error operand {name} undecodable: {e}"))
                })
            };
            let (left, right) = (operand("left")?, operand("right")?);
            ordered_compare_error(domain, &left, &right).ok_or_else(|| {
                PgError::InvalidState(format!(
                    "the compiled check reported an error the kernel does not raise for {left:?} vs {right:?}"
                ))
            })
        }
        other => Err(PgError::InvalidState(format!(
            "error kind {other} is not one the compiler renders"
        ))),
    }
}

/// One index the compiled SQL can seek on: a partial expression index over
/// one argument position of one predicate. Built from the same
/// [`Representation`] as the query's extractor, so the two always match.
/// Correctness never depends on it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct IndexSpec {
    pub(crate) predicate: PredicateName,
    pub(crate) position: usize,
    pub(crate) representation: Representation,
    /// The indexed expression, unqualified, as it appears inside the
    /// parentheses of `CREATE INDEX`.
    pub(crate) expression_sql: String,
    /// The partial-index predicate.
    pub(crate) partial_predicate_sql: String,
}

impl IndexSpec {
    fn new(predicate: PredicateName, position: usize, representation: Representation) -> Self {
        let expression_sql = representation.extractor("", position);
        let partial_predicate_sql =
            format!("predicate_name = {}", quote_literal(predicate.as_str()));
        Self {
            predicate,
            position,
            representation,
            expression_sql,
            partial_predicate_sql,
        }
    }

    /// The digest of the whole canonical specification: same digest,
    /// same physical requirement.
    pub(crate) fn digest(&self) -> String {
        use sha2::{Digest as _, Sha256};
        let canonical = format!(
            "morpholog.claims\nbtree\n{}\n{}\n{}\n{}\n{}\n",
            self.predicate,
            self.position,
            self.representation.as_str(),
            self.expression_sql,
            self.partial_predicate_sql
        );
        hex::encode(Sha256::digest(canonical.as_bytes()))
    }

    /// The deterministic name: a readable reserved prefix plus the digest,
    /// well under the identifier limit.
    pub(crate) fn index_name(&self) -> String {
        let readable: String = self
            .predicate
            .as_str()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .take(24)
            .collect();
        format!(
            "morpholog_ci_{readable}_{}_{}_{}",
            self.position,
            self.representation.as_str(),
            &self.digest()[..12]
        )
    }

    /// The build statement, concurrent so the table stays writable; it
    /// cannot run inside a transaction.
    pub(crate) fn create_sql(&self) -> String {
        format!(
            "CREATE INDEX CONCURRENTLY {} ON morpholog.claims USING btree (({})) WHERE {}",
            quote_ident(&self.index_name()),
            self.expression_sql,
            self.partial_predicate_sql
        )
    }
}

/// How much of a compiled invariant a transition's delta touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CaseFilter {
    /// Delta disjoint from the invariant's occurrences: skip it entirely.
    Untouched,
    /// The touched cases, as a SQL disjunction over the antecedent's
    /// columns, spliced into the full query.
    Bounded(String),
    /// Touched, but not boundable to antecedent columns: run the full check.
    Unbounded,
}

#[derive(Debug, Clone)]
struct ColRef {
    alias: String,
    predicate: PredicateName,
    position: usize,
    kind: PredicateArgKind,
}

#[derive(Debug)]
pub(crate) struct CompiledInvariant {
    pub(crate) name: InvariantName,
    pub(crate) version: u32,
    /// Witness variables, sorted by name. Each violation row carries the
    /// full tagged value as `w_<var>`.
    pub(crate) witness_vars: Vec<Var>,
    /// Which cases a delta touches, decided by core; `case_cols` renders
    /// its bindings onto the antecedent's columns.
    plan: ImpactPlan,
    case_cols: BTreeMap<Var, ColRef>,
    sql_select_from_where: String,
    sql_order_limit: String,
    /// The first binding in scope the kernel would raise on, asked only
    /// after a violation row was found: the violation query stops at its
    /// first row, and an error must win over a violation that sorts
    /// earlier. Has no `LIMIT`, so a case filter can bound it like the
    /// violation query. `None` when nothing in the body can raise.
    error: Option<ErrorQuery>,
    /// The indexes this invariant's SQL can seek on, in specification
    /// order.
    pub(crate) required_indexes: Vec<IndexSpec>,
}

/// The error query without its order: the order depends on the
/// transitions in flight, so the runner renders it.
#[derive(Debug)]
struct ErrorQuery {
    select_from_where: String,
    /// The claim aliases of the scope, in the kernel's nesting order.
    aliases: Vec<String>,
}

impl CompiledInvariant {
    /// The violation query. `case_filter` is a bound from
    /// [`Self::case_filter`]; `None` is the full check.
    pub(crate) fn violation_sql(&self, case_filter: Option<&str>) -> String {
        let stage = if case_filter.is_some() { 2 } else { 1 };
        let mut sql = format!(
            "/* morpholog compiled invariant {} v{} stage{} */\n{}",
            comment_safe(self.name.as_str()),
            self.version,
            stage,
            self.sql_select_from_where
        );
        if let Some(filter) = case_filter {
            let _ = write!(sql, "\n  AND ({filter})");
        }
        sql.push_str(&self.sql_order_limit);
        sql
    }

    /// The error query over the same obligation as the violation query,
    /// ordered as the kernel evaluates: rows loaded before the
    /// transitions, then each step's admissions in statement order, alias
    /// by alias in the scope's nesting order.
    pub(crate) fn error_sql(
        &self,
        case_filter: Option<&str>,
        steps: &[DeltaStep],
    ) -> Result<Option<String>, PgError> {
        let Some(query) = &self.error else {
            return Ok(None);
        };
        let mut sql = query.select_from_where.clone();
        if let Some(filter) = case_filter {
            let _ = write!(sql, "\n  AND ({filter})");
        }
        let keys = query
            .aliases
            .iter()
            .map(|alias| order_keys(alias, steps))
            .collect::<Result<Vec<_>, _>>()?
            .join(", ");
        let _ = write!(sql, "\nORDER BY {keys}\nLIMIT 1");
        Ok(Some(sql))
    }

    /// Bound the check to the cases a delta could have changed: core
    /// decides the cases, this renders them. A value the SQL cannot
    /// compare widens to the whole invariant, never narrows.
    pub(crate) fn case_filter(
        &self,
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
    ) -> CaseFilter {
        let cases = match self.plan.classify(asserted, retracted) {
            Impact::Untouched => return CaseFilter::Untouched,
            Impact::Unbounded => return CaseFilter::Unbounded,
            Impact::Bounded(cases) => cases,
        };
        let mut disjuncts: BTreeSet<String> = BTreeSet::new();
        for case in &cases {
            let mut parts = Vec::new();
            for (var, ev) in case {
                let Some(col) = self.case_cols.get(var) else {
                    return CaseFilter::Unbounded;
                };
                match const_eq(col, ev) {
                    Some(sql) => parts.push(sql),
                    None => return CaseFilter::Unbounded,
                }
            }
            disjuncts.insert(parts.join(" AND "));
        }
        if disjuncts.is_empty() {
            return CaseFilter::Bounded("(false)".to_string());
        }
        let filter = disjuncts.into_iter().collect::<Vec<_>>().join(") OR (");
        CaseFilter::Bounded(format!("({filter})"))
    }
}

/// An invariant name can be any string in hand-built IR, so neutralise
/// anything that could break the block comment it sits in. PostgreSQL
/// block comments NEST, so `/*` is as dangerous as `*/`: it would leave
/// the comment open over the rest of the statement.
fn comment_safe(name: &str) -> String {
    name.replace(['\r', '\n'], " ")
        .replace("*/", "* /")
        .replace("/*", "/ *")
}

/// Compile every invariant of a validated programme, or report every
/// refusal: nothing compiles unless everything does.
pub(crate) fn compile_invariants(
    program: ValidatedProgram<'_>,
) -> Result<CompiledInvariantSet, Vec<CompileRefusal>> {
    let program = program.as_program();
    let decls: BTreeMap<&str, &PredicateDecl> = program
        .predicates
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();
    let mut compiled = Vec::new();
    let mut refusals = Vec::new();
    for inv in &program.invariants {
        match compile_invariant(inv, &decls) {
            Ok(c) => compiled.push(c),
            Err(reason) => refusals.push(CompileRefusal {
                invariant: inv.name.clone(),
                reason,
            }),
        }
    }
    if refusals.is_empty() {
        Ok(CompiledInvariantSet {
            invariants: compiled,
        })
    } else {
        Err(refusals)
    }
}

type Env = BTreeMap<Var, ColRef>;

struct Ctx<'a> {
    decls: &'a BTreeMap<&'a str, &'a PredicateDecl>,
    counter: usize,
    /// Every (predicate, position, representation) the SQL filters or
    /// joins on, collected where the extractor is emitted: the index
    /// specification.
    required: BTreeSet<(PredicateName, usize, Representation)>,
    /// Sums rendered while a comparison's operands were being rendered:
    /// the comparison collects them into its own rendering.
    pending_sums: Vec<RenderedSum>,
}

/// One sum, computed once per row of the scope it sits in as a LATERAL
/// item, with the representability test of its total.
struct RenderedSum {
    lateral: String,
    range_error: String,
}

/// A place the kernel could raise, as data: the condition on a row of the
/// scope, and what to report when it is the first such row.
#[derive(Debug, Clone)]
struct RenderedError {
    condition: String,
    report: ErrorReport,
}

#[derive(Debug, Clone)]
enum ErrorReport {
    /// A sum's total no decimal can hold.
    SumRange,
    /// Two operands the kernel cannot order: the tagged values, as jsonb
    /// expressions, for the kernel to judge.
    Compare {
        domain: OrderedDomain,
        left: String,
        right: String,
    },
}

/// A scope's rendering. `where_` holds the conjuncts before any
/// error-bearing comparison; `tail` is that comparison (the scope's last
/// conjunct), with its sums in `laterals` and its ways of raising in
/// `errors`. The tail is kept apart so an error counts only where the
/// kernel would evaluate the comparison: past the prefix, and then ahead
/// of the comparison's own result.
#[derive(Default)]
struct Rendered {
    from: Vec<(String, String)>, // (alias, from item)
    laterals: Vec<String>,
    where_: Vec<String>,
    tail: Option<String>,
    errors: Vec<RenderedError>,
    env: Env,
}

impl Rendered {
    fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// The prefix conjuncts, or `true` when there are none.
    fn prefix(&self) -> String {
        if self.where_.is_empty() {
            "true".to_string()
        } else {
            self.where_.join(" AND ")
        }
    }

    /// Any of this scope's ways of raising, or `false`.
    fn error_any(&self) -> String {
        or_all(
            &self
                .errors
                .iter()
                .map(|e| e.condition.clone())
                .collect::<Vec<_>>(),
        )
    }

    fn conjunction(&self) -> String {
        self.where_
            .iter()
            .chain(self.tail.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    /// `EXISTS`-shaped rendering of this match, usable inside a WHERE.
    /// Never reached with an error-bearing tail in scope: those shapes
    /// are refused first.
    fn exists_sql(&self) -> String {
        if self.from.is_empty() && self.laterals.is_empty() {
            format!("({})", self.conjunction())
        } else {
            format!(
                "EXISTS (SELECT 1 FROM {} WHERE {})",
                from_list(self),
                self.conjunction()
            )
        }
    }
}

/// The typed refusal for an error-bearing tail where the fragment has no
/// place for one: a sum's, unless a comparison itself can raise.
fn shape_refusal(r: &Rendered, sum: &'static str, comparison: &'static str) -> CompileReason {
    if r.errors
        .iter()
        .any(|e| matches!(e.report, ErrorReport::Compare { .. }))
    {
        CompileReason::ComparisonShape { detail: comparison }
    } else {
        CompileReason::SumShape { detail: sum }
    }
}

/// Conjunction without the `true` an absent part contributes; `false`
/// outright when any part is.
fn and_all(parts: &[String]) -> String {
    if parts.iter().any(|p| p == "false") {
        return "false".to_string();
    }
    let live: Vec<&str> = parts
        .iter()
        .map(String::as_str)
        .filter(|p| *p != "true")
        .collect();
    match live.len() {
        0 => "true".to_string(),
        1 => live[0].to_string(),
        _ => format!("({})", live.join(" AND ")),
    }
}

/// Disjunction without the `false` an absent part contributes; `true`
/// outright when any part is.
fn or_all(parts: &[String]) -> String {
    if parts.iter().any(|p| p == "true") {
        return "true".to_string();
    }
    let live: Vec<&str> = parts
        .iter()
        .map(String::as_str)
        .filter(|p| *p != "false")
        .collect();
    match live.len() {
        0 => "false".to_string(),
        1 => live[0].to_string(),
        _ => format!("({})", live.join(" OR ")),
    }
}

/// True when the total is NOT a representable decimal. It is representable
/// iff, normalised, its scale is at most 28 and its coefficient fits 96
/// bits, the kernel's own test. Data, never a thrown error, so the
/// planner's evaluation order cannot change what the query reports.
fn range_error_sql(total: &str) -> String {
    format!(
        "NOT (min_scale({total}) <= 28 AND abs({total}) * power(10::numeric, min_scale({total})) < 79228162514264337593543950336::numeric)"
    )
}

fn compile_invariant(
    inv: &Invariant,
    decls: &BTreeMap<&str, &PredicateDecl>,
) -> Result<CompiledInvariant, CompileReason> {
    let mut ctx = Ctx {
        decls,
        counter: 0,
        required: BTreeSet::new(),
        pending_sums: Vec::new(),
    };

    let (select_from_where, order_limit, error, case_cols) = match &inv.body {
        Prop::Implies { left, right } => compile_denial(left, right, &mut ctx)?,
        Prop::Forall {
            binding: _,
            source,
            body,
        } => compile_denial(source, body, &mut ctx)?,
        // Top-level Not: violated iff the inner matches. The inner's
        // bindings bound the cases, but the kernel reports no witness here,
        // so neither does the check.
        Prop::Not(inner) => {
            let r = render_prop(inner, Env::new(), &mut ctx)?;
            if r.from.is_empty() {
                generic_denial(&inv.body, &mut ctx)?
            } else {
                // Violated iff the inner matches: past the prefix, an
                // error or a holding tail.
                let (select, order) = witness_select_order(&r);
                let violated = or_all(&[
                    r.error_any(),
                    r.tail.clone().unwrap_or_else(|| "true".to_string()),
                ]);
                let mut where_ = r.where_.clone();
                if violated != "true" {
                    where_.push(violated);
                }
                (
                    format!(
                        "SELECT {select}\nFROM {}\nWHERE {}",
                        from_list(&r),
                        where_.join("\n  AND ")
                    ),
                    format!("\nORDER BY {order}\nLIMIT 1"),
                    error_query(&r),
                    r.env,
                )
            }
        }
        other => generic_denial(other, &mut ctx)?,
    };

    let witness_vars = match &inv.body {
        Prop::Not(_) => Vec::new(),
        _ => case_cols.keys().cloned().collect(),
    };
    let plan = ImpactPlan::new(inv);
    Ok(CompiledInvariant {
        name: inv.name.clone(),
        version: inv.version,
        witness_vars,
        plan,
        case_cols,
        sql_select_from_where: select_from_where,
        sql_order_limit: order_limit,
        error,
        required_indexes: ctx
            .required
            .iter()
            .map(|(predicate, position, repr)| IndexSpec::new(predicate.clone(), *position, *repr))
            .collect(),
    })
}

/// The dominant shape: `antecedent implies consequent`. Violation = an
/// antecedent match with no consequent match.
type Denial = (String, String, Option<ErrorQuery>, Env);

fn compile_denial(left: &Prop, right: &Prop, ctx: &mut Ctx<'_>) -> Result<Denial, CompileReason> {
    let ant = render_prop(left, Env::new(), ctx)?;
    if ant.from.is_empty() {
        // Filter-only antecedent: nothing to witness, so use the generic
        // denial.
        return generic_denial_implies(left, right, ctx);
    }
    let cons = render_prop(right, ant.env.clone(), ctx)?;
    if cons.has_errors() && !cons.from.is_empty() {
        return Err(shape_refusal(
            &cons,
            "sum beside claim patterns in a consequent",
            "a quantity or timestamp ordering beside claim patterns in a consequent",
        ));
    }
    // Past the antecedent's prefix, a violation is the antecedent's error,
    // or its tail holding and the consequent failing. A consequent's error
    // counts only once the antecedent's tail and the consequent's prefix
    // hold; a failing prefix is an ordinary violation that never reaches
    // the comparison.
    let ant_tail = ant.tail.clone().unwrap_or_else(|| "true".to_string());
    let mut errors = ant.errors.clone();
    for e in &cons.errors {
        errors.push(RenderedError {
            condition: and_all(&[ant_tail.clone(), cons.prefix(), e.condition.clone()]),
            report: e.report.clone(),
        });
    }
    let not_cons = if cons.has_errors() {
        format!(
            "NOT {}",
            and_all(&[
                cons.prefix(),
                cons.tail.clone().unwrap_or_else(|| "true".to_string())
            ])
        )
    } else if cons.from.is_empty() {
        format!("NOT ({})", cons.conjunction())
    } else {
        format!("NOT {}", cons.exists_sql())
    };
    let mut scope = Rendered {
        from: ant.from.clone(),
        laterals: ant.laterals.clone(),
        where_: ant.where_.clone(),
        errors,
        ..Rendered::default()
    };
    scope.laterals.extend(cons.laterals.iter().cloned());
    let (select, order) = witness_select_order(&ant);
    let mut where_ = ant.where_.clone();
    where_.push(or_all(&[scope.error_any(), and_all(&[ant_tail, not_cons])]));
    Ok((
        format!(
            "SELECT {select}\nFROM {}\nWHERE {}",
            from_list(&scope),
            where_.join("\n  AND ")
        ),
        format!("\nORDER BY {order}\nLIMIT 1"),
        error_query(&scope),
        ant.env,
    ))
}

/// The rows of the scope the kernel would raise on, with what to report
/// for the first: which way it raised, and for a comparison, its domain
/// and operands. `None` when nothing in scope can raise.
fn error_query(scope: &Rendered) -> Option<ErrorQuery> {
    if !scope.has_errors() {
        return None;
    }
    let case = |pick: &dyn Fn(&RenderedError) -> Option<String>| {
        let arms: Vec<String> = scope
            .errors
            .iter()
            .filter_map(|e| pick(e).map(|v| format!(" WHEN {} THEN {v}", e.condition)))
            .collect();
        if arms.is_empty() {
            "NULL::text".to_string()
        } else {
            format!("CASE{} END", arms.concat())
        }
    };
    let kind = case(&|e| {
        Some(match e.report {
            ErrorReport::SumRange => "'range'".to_string(),
            ErrorReport::Compare { .. } => "'compare'".to_string(),
        })
    });
    let domain = case(&|e| match &e.report {
        ErrorReport::SumRange => None,
        ErrorReport::Compare { domain, .. } => Some(match domain {
            OrderedDomain::Decimal => "'decimal'".to_string(),
            OrderedDomain::Timestamp => "'timestamp'".to_string(),
            OrderedDomain::Date | OrderedDomain::Duration => unreachable!("never rendered"),
        }),
    });
    let left = case(&|e| match &e.report {
        ErrorReport::SumRange => None,
        ErrorReport::Compare { left, .. } => Some(format!("({left})::text")),
    });
    let right = case(&|e| match &e.report {
        ErrorReport::SumRange => None,
        ErrorReport::Compare { right, .. } => Some(format!("({right})::text")),
    });
    Some(ErrorQuery {
        select_from_where: format!(
            "SELECT {kind} AS \"kind\",\n       {domain} AS \"domain\",\n       {left} AS \"left\",\n       {right} AS \"right\"\nFROM {}\nWHERE {}",
            from_list(scope),
            and_all(&[scope.prefix(), scope.error_any()])
        ),
        aliases: scope.from.iter().map(|(alias, _)| alias.clone()).collect(),
    })
}

/// Any other top-level shape: the invariant holds iff the body matches at
/// all, so violation is bare non-existence, with an empty witness.
fn generic_denial(body: &Prop, ctx: &mut Ctx<'_>) -> Result<Denial, CompileReason> {
    let r = render_prop(body, Env::new(), ctx)?;
    if !r.has_errors() {
        return Ok((
            format!("SELECT 1 AS \"w\"\nWHERE NOT {}", r.exists_sql()),
            String::new(),
            None,
            Env::new(),
        ));
    }
    // The kernel evaluates the comparison for every prefix match, so an
    // error anywhere dominates; otherwise the body must match somewhere.
    let scope = |condition: String| {
        format!(
            "EXISTS (SELECT 1 FROM {} WHERE {})",
            from_list(&r),
            and_all(&[r.prefix(), condition])
        )
    };
    let raises = scope(r.error_any());
    let holds = scope(r.tail.clone().unwrap_or_else(|| "true".to_string()));
    Ok((
        format!("SELECT 1 AS \"w\"\nWHERE {raises} OR NOT {holds}"),
        String::new(),
        error_query(&r),
        Env::new(),
    ))
}

fn generic_denial_implies(
    left: &Prop,
    right: &Prop,
    ctx: &mut Ctx<'_>,
) -> Result<Denial, CompileReason> {
    let l = render_prop(left, Env::new(), ctx)?;
    let r = render_prop(right, l.env.clone(), ctx)?;
    if l.has_errors() || r.has_errors() {
        let raising = if l.has_errors() { &l } else { &r };
        return Err(shape_refusal(
            raising,
            "sum in a filter-only implication",
            "a quantity or timestamp ordering in a filter-only implication",
        ));
    }
    let not_r = if r.from.is_empty() {
        format!("NOT ({})", r.conjunction())
    } else {
        format!("NOT {}", r.exists_sql())
    };
    let mut where_ = l.where_.clone();
    where_.push(not_r);
    let violated = Rendered {
        from: l.from,
        where_,
        ..Rendered::default()
    };
    Ok((
        format!("SELECT 1 AS \"w\"\nWHERE {}", violated.exists_sql()),
        String::new(),
        None,
        Env::new(),
    ))
}

fn from_list(r: &Rendered) -> String {
    r.from
        .iter()
        .map(|(_, f)| f.as_str())
        .chain(r.laterals.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(", ")
}

fn witness_select_order(r: &Rendered) -> (String, String) {
    let select = if r.env.is_empty() {
        "1 AS \"w\"".to_string()
    } else {
        // The full tagged value, so decoding goes through EvalValue's
        // serde and needs no per-kind column logic.
        r.env
            .iter()
            .map(|(v, col)| {
                format!(
                    "({}.arguments -> {})::text AS {}",
                    col.alias,
                    col.position,
                    quote_ident(&format!("w_{v}"))
                )
            })
            .collect::<Vec<_>>()
            .join(",\n       ")
    };
    // Order by the extractor expressions, not raw `arguments`. Raw order
    // matches the primary key, which tempts the planner into an early-stop
    // scan of the whole predicate (seen at 100k rows); the extractors match
    // the partial expression indexes the compiler emits. Deterministic in
    // everything the row reports.
    let order = if r.env.is_empty() {
        r.from
            .iter()
            .map(|(alias, _)| format!("{alias}.arguments"))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        r.env
            .values()
            .map(|col| format!("({})::text", col_sql(col, Representation::Text)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    (select, order)
}

/// One expression over a claim position that a btree index can be built
/// over and the SQL can seek on. An [`Equality`] names the one it seeks
/// with, so the query and the index always match. The module doc gives
/// the reason per kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Representation {
    Text,
    Numeric,
    /// The whole tagged value; sound only for kinds whose canonical
    /// serialisation makes structural equality semantic equality.
    Jsonb,
    /// A quantity's amount as `numeric`; its unit is a residual condition.
    QuantityAmount,
}

impl Representation {
    /// The extractor over `arguments` at `position`. `qualifier` is the
    /// table alias followed by a dot in a query, empty in an index
    /// expression.
    pub(crate) fn extractor(self, qualifier: &str, position: usize) -> String {
        match self {
            Representation::Text => format!("{qualifier}arguments -> {position} ->> 'value'"),
            Representation::Numeric => {
                format!("({qualifier}arguments -> {position} ->> 'value')::numeric")
            }
            Representation::Jsonb => format!("{qualifier}arguments -> {position}"),
            Representation::QuantityAmount => {
                format!("({qualifier}arguments -> {position} -> 'value' ->> 'amount')::numeric")
            }
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Representation::Text => "text",
            Representation::Numeric => "numeric",
            Representation::Jsonb => "jsonb",
            Representation::QuantityAmount => "quantity_amount",
        }
    }
}

/// How two claim positions of one declared kind are read so that SQL
/// equality is kernel equality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Equality {
    Text,
    Numeric,
    Jsonb,
    /// Amount as `numeric` and unit as text, both agreeing.
    Quantity,
}

impl Equality {
    fn for_kind(kind: &PredicateArgKind) -> Result<Self, CompileReason> {
        match kind {
            PredicateArgKind::Decimal => Ok(Equality::Numeric),
            PredicateArgKind::Subject => Ok(Equality::Text),
            PredicateArgKind::Bool
            | PredicateArgKind::Date
            | PredicateArgKind::Timestamp
            | PredicateArgKind::Duration => Ok(Equality::Jsonb),
            PredicateArgKind::Quantity(_) => Ok(Equality::Quantity),
            PredicateArgKind::Collection
            | PredicateArgKind::Any
            | PredicateArgKind::CalendarSpan => {
                Err(CompileReason::ArgumentKind { kind: kind.clone() })
            }
        }
    }

    /// The expression an index serves this equality with.
    fn seek(self) -> Representation {
        match self {
            Equality::Text => Representation::Text,
            Equality::Numeric => Representation::Numeric,
            Equality::Jsonb => Representation::Jsonb,
            Equality::Quantity => Representation::QuantityAmount,
        }
    }

    fn col_eq(self, a: &ColRef, b: &ColRef) -> String {
        match self {
            Equality::Quantity => format!(
                "({}) = ({}) AND ({}) = ({})",
                col_sql(a, Representation::QuantityAmount),
                col_sql(b, Representation::QuantityAmount),
                unit_sql(a),
                unit_sql(b)
            ),
            other => format!(
                "({}) = ({})",
                col_sql(a, other.seek()),
                col_sql(b, other.seek())
            ),
        }
    }
}

fn col_sql(col: &ColRef, repr: Representation) -> String {
    repr.extractor(&format!("{}.", col.alias), col.position)
}

/// A quantity position's unit, text; NULL for any other stored kind.
fn unit_sql(col: &ColRef) -> String {
    format!(
        "{}.arguments -> {} -> 'value' ->> 'unit'",
        col.alias, col.position
    )
}

/// Whether the stored value at a position carries `tag`, never NULL.
fn tagged_as_sql(col: &ColRef, tag: &str) -> String {
    format!(
        "COALESCE(({}.arguments -> {} ->> 'type') = {}, false)",
        col.alias,
        col.position,
        quote_literal(tag)
    )
}

/// The whole tagged value at a position, jsonb.
fn tagged_sql(col: &ColRef) -> String {
    format!("{}.arguments -> {}", col.alias, col.position)
}

/// A literal in a claim pattern: the position must hold exactly it.
/// Refuses the literal kinds the fragment cannot render as constants.
fn literal_filter(col: &ColRef, value: &Value, ctx: &mut Ctx<'_>) -> Result<String, CompileReason> {
    let mut require = |repr: Representation| {
        ctx.required
            .insert((col.predicate.clone(), col.position, repr));
    };
    match value {
        Value::Subject(s) => {
            require(Representation::Text);
            Ok(format!(
                "({}) = {}",
                col_sql(col, Representation::Text),
                quote_literal(s.as_str())
            ))
        }
        Value::Decimal(d) => {
            require(Representation::Numeric);
            Ok(format!(
                "({}) = {}::numeric",
                col_sql(col, Representation::Numeric),
                quote_literal(d)
            ))
        }
        Value::Quantity { .. } => {
            let EvalValue::Quantity { amount, unit } = literal_eval(value)? else {
                unreachable!("a quantity literal evaluates to a quantity")
            };
            require(Representation::QuantityAmount);
            Ok(format!(
                "({}) = {}::numeric AND ({}) = {}",
                col_sql(col, Representation::QuantityAmount),
                quote_literal(&amount.to_string()),
                unit_sql(col),
                quote_literal(unit.as_str())
            ))
        }
        Value::Timestamp(_) => {
            let ev = literal_eval(value)?;
            require(Representation::Jsonb);
            Ok(format!(
                "({}) = {}::jsonb",
                col_sql(col, Representation::Jsonb),
                quote_literal(&tagged_json(&ev)?)
            ))
        }
        Value::Date(_) => Err(CompileReason::Literal { kind: "date" }),
        Value::Duration(_) => Err(CompileReason::Literal { kind: "duration" }),
        Value::CalendarSpan(_) => Err(CompileReason::Literal {
            kind: "calendar span",
        }),
    }
}

/// A literal's runtime value, as the evaluator would parse it. A literal
/// a validated programme carries always parses.
fn literal_eval(value: &Value) -> Result<EvalValue, CompileReason> {
    literal_value(value).map_err(|e| CompileReason::UnvalidatedShape {
        detail: format!("literal does not evaluate: {e}"),
    })
}

/// The canonical tagged JSON of a runtime value: the same serde every
/// stored claim passed through.
fn tagged_json(ev: &EvalValue) -> Result<String, CompileReason> {
    serde_json::to_string(ev).map_err(|e| CompileReason::UnvalidatedShape {
        detail: format!("value does not serialise: {e}"),
    })
}

/// Case-bound constant equality on an antecedent column, or None when the
/// value kind cannot be rendered (widens to Unbounded). Tagged kinds
/// compare as jsonb against the value's own serialisation, the same serde
/// every stored claim passed through.
fn const_eq(col: &ColRef, ev: &EvalValue) -> Option<String> {
    match ev {
        EvalValue::Subject(s) => Some(format!(
            "({}) = {}",
            col_sql(col, Representation::Text),
            quote_literal(s.as_str())
        )),
        EvalValue::Decimal(d) => Some(format!(
            "({}) = {}::numeric",
            col_sql(col, Representation::Numeric),
            quote_literal(&d.to_string())
        )),
        EvalValue::Quantity { amount, unit } => Some(format!(
            "({}) = {}::numeric AND ({}) = {}",
            col_sql(col, Representation::QuantityAmount),
            quote_literal(&amount.to_string()),
            unit_sql(col),
            quote_literal(unit.as_str())
        )),
        EvalValue::Bool(_)
        | EvalValue::Date(_)
        | EvalValue::Timestamp(_)
        | EvalValue::Duration(_) => {
            let json = serde_json::to_string(ev).ok()?;
            Some(format!(
                "({}) = {}::jsonb",
                col_sql(col, Representation::Jsonb),
                quote_literal(&json)
            ))
        }
        EvalValue::Collection(_) | EvalValue::CalendarSpan(_) => None,
    }
}

fn render_prop(prop: &Prop, env: Env, ctx: &mut Ctx<'_>) -> Result<Rendered, CompileReason> {
    match prop {
        Prop::Claim { predicate, args } => render_claim(predicate, args, env, ctx),
        Prop::And(ps) => {
            let mut acc = Rendered {
                env,
                ..Rendered::default()
            };
            for p in ps {
                if acc.tail.is_some() {
                    // The kernel evaluates a later conjunct only for
                    // bindings the comparison admitted; the query computes
                    // every row's sum, and reports an error wherever its
                    // condition holds. Only a closing comparison has one
                    // evaluation boundary.
                    return Err(shape_refusal(
                        &acc,
                        "a sum comparison must be the last conjunct of its scope",
                        "a quantity or timestamp ordering must be the last conjunct of its scope",
                    ));
                }
                let r = render_prop(p, acc.env.clone(), ctx)?;
                acc.from.extend(r.from);
                acc.laterals.extend(r.laterals);
                acc.where_.extend(r.where_);
                acc.errors.extend(r.errors);
                acc.tail = r.tail;
                acc.env = r.env;
            }
            Ok(acc)
        }
        Prop::Not(inner) => {
            let r = nested_scope(render_prop(inner, env.clone(), ctx)?)?;
            let clause = if r.from.is_empty() {
                format!("NOT ({})", r.conjunction())
            } else {
                format!("NOT {}", r.exists_sql())
            };
            Ok(Rendered {
                where_: vec![clause],
                env,
                ..Rendered::default()
            })
        }
        Prop::Exists { binding: _, body } => {
            let r = nested_scope(render_prop(body, env.clone(), ctx)?)?;
            Ok(Rendered {
                where_: vec![r.exists_sql()],
                env,
                ..Rendered::default()
            })
        }
        Prop::Implies { left, right } => {
            let l = nested_scope(render_prop(left, env.clone(), ctx)?)?;
            let r = nested_scope(render_prop(right, l.env.clone(), ctx)?)?;
            let not_r = if r.from.is_empty() {
                format!("NOT ({})", r.conjunction())
            } else {
                format!("NOT {}", r.exists_sql())
            };
            let mut where_ = l.where_;
            where_.push(not_r);
            let violated = Rendered {
                from: l.from,
                where_,
                ..Rendered::default()
            };
            Ok(Rendered {
                where_: vec![format!("NOT {}", violated.exists_sql())],
                env,
                ..Rendered::default()
            })
        }
        Prop::Forall {
            binding: _,
            source,
            body,
        } => render_prop(
            &Prop::Implies {
                left: source.clone(),
                right: body.clone(),
            },
            env,
            ctx,
        ),
        Prop::Eq(a, b) => equality_sql(a, b, false, &env, ctx),
        Prop::Neq(a, b) => equality_sql(a, b, true, &env, ctx),
        Prop::Compare {
            op,
            domain,
            left,
            right,
        } => match domain {
            OrderedDomain::Decimal | OrderedDomain::Timestamp => {
                ordered_sql(*op, *domain, left, right, &env, ctx)
            }
            OrderedDomain::Date | OrderedDomain::Duration => {
                Err(CompileReason::ComparisonDomain { domain: *domain })
            }
        },
        Prop::Or(_) => Err(CompileReason::Construct { construct: "or" }),
        Prop::Xor(_, _) => Err(CompileReason::Construct { construct: "xor" }),
        Prop::Pre(_) => Err(CompileReason::Construct { construct: "pre" }),
        Prop::Defined { .. } => Err(CompileReason::Construct {
            construct: "defined call",
        }),
        Prop::In(_, _) => Err(CompileReason::Construct { construct: "in" }),
    }
}

fn render_claim(
    predicate: &PredicateName,
    args: &[Term],
    mut env: Env,
    ctx: &mut Ctx<'_>,
) -> Result<Rendered, CompileReason> {
    let decl =
        ctx.decls
            .get(predicate.as_str())
            .ok_or_else(|| CompileReason::UnvalidatedShape {
                detail: format!("undeclared predicate {predicate}"),
            })?;
    let alias = format!("t{}", ctx.counter);
    ctx.counter += 1;

    let mut where_ = vec![format!(
        "{alias}.predicate_name = {}",
        quote_literal(predicate.as_str())
    )];
    for (i, term) in args.iter().enumerate() {
        let kind = decl.args.get(i).map(|a| a.kind.clone()).ok_or_else(|| {
            CompileReason::UnvalidatedShape {
                detail: format!("arity mismatch on {predicate}"),
            }
        })?;
        let col = ColRef {
            alias: alias.clone(),
            predicate: predicate.clone(),
            position: i,
            kind,
        };
        match term {
            Term::Wildcard => {}
            Term::Actor => {
                return Err(CompileReason::Construct { construct: "actor" });
            }
            Term::Literal(v) => {
                where_.push(literal_filter(&col, v, ctx)?);
            }
            Term::Var(v) => {
                if let Some(bound) = env.get(v) {
                    let eq = Equality::for_kind(&col.kind)?;
                    where_.push(eq.col_eq(bound, &col));
                    ctx.required
                        .insert((bound.predicate.clone(), bound.position, eq.seek()));
                    ctx.required.insert((predicate.clone(), i, eq.seek()));
                } else {
                    // Binding requires a proved equality representation
                    // now, so a later join or witness read can never fall
                    // back to an unsound comparison.
                    Equality::for_kind(&col.kind)?;
                    env.insert(v.clone(), col);
                }
            }
        }
    }
    Ok(Rendered {
        from: vec![(alias.clone(), format!("morpholog.claims {alias}"))],
        where_,
        env,
        ..Rendered::default()
    })
}

/// A scope checked by existence (nested negation, exists, implication)
/// may stop at its first match, while the kernel evaluates every binding
/// of a scope that can raise, so no error-bearing tail compiles under one.
fn nested_scope(r: Rendered) -> Result<Rendered, CompileReason> {
    if r.has_errors() {
        return Err(shape_refusal(
            &r,
            "sum under a nested scope",
            "a quantity or timestamp ordering under a nested scope",
        ));
    }
    Ok(r)
}

/// A value in comparison position, by the flavour the SQL must handle.
enum Operand {
    /// A bare decimal, a decimal literal, or a sum's total: `numeric`.
    Numeric(String),
    /// An equality-only reading (subject text, or a canonical tagged
    /// value as jsonb) of a kind nothing orders here.
    Keyed(String),
    Quantity {
        amount: String,
        unit: String,
        /// SQL boolean: the value is a quantity. `true` for a literal.
        well_typed: String,
        /// The whole tagged value, jsonb.
        tagged: String,
    },
    Timestamp {
        nanos: String,
        well_typed: String,
        tagged: String,
    },
}

/// `a = b`, or its negation: structural equality, which never raises. A
/// pending sum still makes it the scope's tail.
fn equality_sql(
    a: &ValueExpr,
    b: &ValueExpr,
    negated: bool,
    env: &Env,
    ctx: &mut Ctx<'_>,
) -> Result<Rendered, CompileReason> {
    let l = value_sql(a, env, ctx)?;
    let r = value_sql(b, env, ctx)?;
    let clause = match (&l, &r) {
        (Operand::Numeric(x), Operand::Numeric(y)) | (Operand::Keyed(x), Operand::Keyed(y)) => {
            format!("({x}) = ({y})")
        }
        (
            Operand::Quantity {
                amount: xa,
                unit: xu,
                ..
            },
            Operand::Quantity {
                amount: ya,
                unit: yu,
                ..
            },
        ) => format!("COALESCE(({xa}) = ({ya}) AND ({xu}) = ({yu}), false)"),
        (Operand::Timestamp { tagged: x, .. }, Operand::Timestamp { tagged: y, .. }) => {
            format!("({x}) = ({y})")
        }
        _ => {
            return Err(CompileReason::UnvalidatedShape {
                detail: "equality across value flavours".to_string(),
            });
        }
    };
    let clause = if negated {
        format!("NOT ({clause})")
    } else {
        clause
    };
    Ok(close_comparison(clause, None, env, ctx))
}

/// An ordered comparison. Decimals and sums never raise; quantities raise
/// on two units, timestamps on a stored value of another kind, both
/// carried as data.
fn ordered_sql(
    op: CompareOp,
    domain: OrderedDomain,
    a: &ValueExpr,
    b: &ValueExpr,
    env: &Env,
    ctx: &mut Ctx<'_>,
) -> Result<Rendered, CompileReason> {
    let l = value_sql(a, env, ctx)?;
    let r = value_sql(b, env, ctx)?;
    let op = match op {
        CompareOp::Le => "<=",
        CompareOp::Lt => "<",
        CompareOp::Ge => ">=",
        CompareOp::Gt => ">",
    };
    let (clause, raises) = match (domain, &l, &r) {
        (OrderedDomain::Decimal, Operand::Numeric(x), Operand::Numeric(y)) => {
            (format!("({x}) {op} ({y})"), None)
        }
        (
            OrderedDomain::Decimal,
            Operand::Quantity {
                amount: xa,
                unit: xu,
                well_typed: xw,
                tagged: xt,
            },
            Operand::Quantity {
                amount: ya,
                unit: yu,
                well_typed: yw,
                tagged: yt,
            },
        ) => (
            format!("({xa}) {op} ({ya})"),
            raising(
                and_all(&[xw.clone(), yw.clone(), format!("({xu}) = ({yu})")]),
                domain,
                xt,
                yt,
            ),
        ),
        (
            OrderedDomain::Timestamp,
            Operand::Timestamp {
                nanos: xn,
                well_typed: xw,
                tagged: xt,
            },
            Operand::Timestamp {
                nanos: yn,
                well_typed: yw,
                tagged: yt,
            },
        ) => (
            format!("({xn}) {op} ({yn})"),
            raising(and_all(&[xw.clone(), yw.clone()]), domain, xt, yt),
        ),
        _ => {
            return Err(CompileReason::UnvalidatedShape {
                detail: format!("ordered comparison across value flavours under {domain:?}"),
            });
        }
    };
    Ok(close_comparison(clause, raises, env, ctx))
}

/// The comparison's own way of raising: its operands are not what the
/// kernel can order, unless `well_typed` proves they are.
fn raising(
    well_typed: String,
    domain: OrderedDomain,
    left: &str,
    right: &str,
) -> Option<RenderedError> {
    if well_typed == "true" {
        return None;
    }
    Some(RenderedError {
        condition: format!("NOT {well_typed}"),
        report: ErrorReport::Compare {
            domain,
            left: left.to_string(),
            right: right.to_string(),
        },
    })
}

/// A comparison's rendering: a plain conjunct when nothing can raise,
/// otherwise the scope's tail with its sums and errors, the sums' range
/// errors first, as the kernel evaluates operands before comparing.
fn close_comparison(
    clause: String,
    raises: Option<RenderedError>,
    env: &Env,
    ctx: &mut Ctx<'_>,
) -> Rendered {
    let sums = std::mem::take(&mut ctx.pending_sums);
    let mut errors: Vec<RenderedError> = sums
        .iter()
        .map(|s| RenderedError {
            condition: s.range_error.clone(),
            report: ErrorReport::SumRange,
        })
        .collect();
    errors.extend(raises);
    if errors.is_empty() {
        return Rendered {
            where_: vec![clause],
            env: env.clone(),
            ..Rendered::default()
        };
    }
    Rendered {
        laterals: sums.into_iter().map(|s| s.lateral).collect(),
        tail: Some(clause),
        errors,
        env: env.clone(),
        ..Rendered::default()
    }
}

fn value_sql(expr: &ValueExpr, env: &Env, ctx: &mut Ctx<'_>) -> Result<Operand, CompileReason> {
    match expr {
        ValueExpr::Term(Term::Var(v)) => {
            let col = env.get(v).ok_or_else(|| CompileReason::UnvalidatedShape {
                detail: format!("unbound variable in value position: {v}"),
            })?;
            Ok(match Equality::for_kind(&col.kind)? {
                Equality::Numeric => Operand::Numeric(col_sql(col, Representation::Numeric)),
                Equality::Text => Operand::Keyed(col_sql(col, Representation::Text)),
                Equality::Jsonb if col.kind == PredicateArgKind::Timestamp => Operand::Timestamp {
                    nanos: format!("morpholog.timestamp_nanos({})", tagged_sql(col)),
                    well_typed: tagged_as_sql(col, "timestamp"),
                    tagged: tagged_sql(col),
                },
                Equality::Jsonb => Operand::Keyed(col_sql(col, Representation::Jsonb)),
                Equality::Quantity => Operand::Quantity {
                    amount: col_sql(col, Representation::QuantityAmount),
                    unit: unit_sql(col),
                    well_typed: tagged_as_sql(col, "quantity"),
                    tagged: tagged_sql(col),
                },
            })
        }
        ValueExpr::Term(Term::Literal(v)) => literal_operand(v),
        ValueExpr::Term(Term::Wildcard) => Err(CompileReason::Construct {
            construct: "wildcard value",
        }),
        ValueExpr::Term(Term::Actor) => Err(CompileReason::Construct { construct: "actor" }),
        ValueExpr::Sum { value, body, seed } => {
            if *seed != SumSeed::Decimal {
                return Err(CompileReason::SumShape {
                    detail: "non-decimal seed",
                });
            }
            let r = render_prop(body, env.clone(), ctx)?;
            if r.from.is_empty() {
                return Err(CompileReason::SumShape {
                    detail: "body binds no claims",
                });
            }
            if r.has_errors() {
                return Err(shape_refusal(
                    &r,
                    "sum within a sum body",
                    "a quantity or timestamp ordering within a sum body",
                ));
            }
            // The sum target must be a bound decimal variable or a decimal
            // literal; a computed target refuses.
            let val = match value.as_ref() {
                ValueExpr::Term(Term::Var(v)) => {
                    let col = r
                        .env
                        .get(v)
                        .ok_or_else(|| CompileReason::UnvalidatedShape {
                            detail: format!("sum target unbound: {v}"),
                        })?;
                    if col.kind != PredicateArgKind::Decimal {
                        return Err(CompileReason::SumShape {
                            detail: "target is not a decimal position",
                        });
                    }
                    col_sql(col, Representation::Numeric)
                }
                ValueExpr::Term(Term::Literal(Value::Decimal(d))) => {
                    format!("{}::numeric", quote_literal(d))
                }
                _ => {
                    return Err(CompileReason::SumShape {
                        detail: "computed target",
                    });
                }
            };
            // Computed once per row of the enclosing scope, so the
            // comparison and the representability test read one total.
            let alias = format!("l{}", ctx.counter);
            ctx.counter += 1;
            let total = format!("{alias}.s");
            ctx.pending_sums.push(RenderedSum {
                lateral: format!(
                    "LATERAL (SELECT COALESCE(sum({val}), 0::numeric) AS s FROM {} WHERE {}) {alias}",
                    from_list(&r),
                    r.conjunction()
                ),
                range_error: range_error_sql(&total),
            });
            Ok(Operand::Numeric(total))
        }
        ValueExpr::Arith { .. } => Err(CompileReason::Construct {
            construct: "arithmetic",
        }),
        ValueExpr::Extremum { .. } => Err(CompileReason::Construct {
            construct: "extremum",
        }),
        ValueExpr::ValueOf { .. } => Err(CompileReason::Construct {
            construct: "value lookup",
        }),
        ValueExpr::Cond { .. } => Err(CompileReason::Construct {
            construct: "conditional value",
        }),
        ValueExpr::Call { .. } => Err(CompileReason::Construct {
            construct: "builtin call",
        }),
    }
}

/// A literal in value position: a SQL constant of the kernel's value. A
/// timestamp's coordinate is computed here, not by the database.
fn literal_operand(value: &Value) -> Result<Operand, CompileReason> {
    match value {
        Value::Subject(s) => Ok(Operand::Keyed(quote_literal(s.as_str()))),
        Value::Decimal(d) => Ok(Operand::Numeric(format!("{}::numeric", quote_literal(d)))),
        Value::Quantity { .. } => {
            let ev = literal_eval(value)?;
            let EvalValue::Quantity { amount, unit } = &ev else {
                unreachable!("a quantity literal evaluates to a quantity")
            };
            Ok(Operand::Quantity {
                amount: format!("{}::numeric", quote_literal(&amount.to_string())),
                unit: quote_literal(unit.as_str()),
                well_typed: "true".to_string(),
                tagged: format!("{}::jsonb", quote_literal(&tagged_json(&ev)?)),
            })
        }
        Value::Timestamp(_) => {
            let ev = literal_eval(value)?;
            let EvalValue::Timestamp(t) = &ev else {
                unreachable!("a timestamp literal evaluates to a timestamp")
            };
            Ok(Operand::Timestamp {
                nanos: format!("{}::numeric", t.as_nanosecond()),
                well_typed: "true".to_string(),
                tagged: format!("{}::jsonb", quote_literal(&tagged_json(&ev)?)),
            })
        }
        Value::Date(_) => Err(CompileReason::Literal { kind: "date" }),
        Value::Duration(_) => Err(CompileReason::Literal { kind: "duration" }),
        Value::CalendarSpan(_) => Err(CompileReason::Literal {
            kind: "calendar span",
        }),
    }
}

#[cfg(test)]
mod tests;
