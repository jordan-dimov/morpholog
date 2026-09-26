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
//! Equality is one function, `morpholog.value_key_v1`: a key that is
//! equal exactly when the kernel says two stored values are equal, for
//! every value the codec writes, whatever kind the programme now declares
//! for the position. Every join, literal filter and case filter compares
//! keys: a pattern join or filter by the digest of the key, which the
//! index holds, so an entry stays bounded whatever the value holds and a
//! join's inner side comes back from the index untouched (digest equality
//! is the claims table's own identity); an equality in value position by
//! the key itself, exact, since no index is involved. The function is
//! versioned in its name because the index expression carries it: a key
//! with other semantics is a new function, a new expression and a
//! rebuilt index.
//!
//! Ordered comparisons run over numbers: a decimal or a quantity's amount
//! as `numeric`, a timestamp as `morpholog.timestamp_nanos` (nanoseconds
//! since the epoch, the kernel's own precision). Where the kernel can
//! raise while comparing - two quantities of different units, a stored
//! value of another kind than the comparison expects, which history
//! admitted under an older declaration can hold, or a value a sum cannot
//! take - the SQL carries that as data, the way a sum's range test does:
//! the violation query returns such a row, and a second query then names
//! the first erroring binding in the kernel's own order (state as loaded,
//! the transition's admissions at the tail) with its operands, so the
//! runner reports the kernel's exact error.
//!
//! The ways a body can raise form an error plan beside the truth query:
//! one probe scope per stage of a conjunction, and one scope of the
//! enclosing row for whatever the kernel evaluates whole per row (an
//! implication's consequent, a `forall` body, `exists`, `not`, a nested
//! implication), asked in the kernel's order, first hit wins. Sums keep
//! a narrower fragment: a sum-closing comparison is last in its scope and
//! never under a nested one.
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
    /// An ordered comparison over a position whose declared kind the SQL
    /// cannot order by at compile time (`Any`, a collection).
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
                "ordering over kind {kind} is outside the compiled fragment"
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
            .flat_map(|inv| {
                inv.required_indexes
                    .iter()
                    .chain(&inv.case_indexes)
                    .cloned()
            })
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
            for error_sql in inv.error_sqls(case_filter.as_deref(), steps)? {
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
/// (loaded rows first), its place among that step's admissions of the
/// alias's predicate, then the loaded order. The place is looked up by
/// arguments alone, so only the same predicate's admissions are listed:
/// another predicate can admit the same arguments in the same step.
fn order_keys(
    alias: &str,
    predicate: &PredicateName,
    steps: &[DeltaStep],
) -> Result<String, PgError> {
    if steps.is_empty() {
        return Ok(format!("{alias}.arguments_hash"));
    }
    let mut step_arms = String::new();
    let mut place_arms = String::new();
    for (i, step) in steps.iter().enumerate() {
        let id = quote_literal(&step.transition_id.to_string());
        let _ = write!(step_arms, " WHEN {id} THEN {}", i + 1);
        let admitted = step
            .asserted
            .iter()
            .filter(|c| c.predicate == *predicate)
            .map(|c| {
                Ok(format!(
                    "{}::jsonb",
                    quote_literal(&serde_json::to_string(&c.args)?)
                ))
            })
            .collect::<Result<Vec<_>, PgError>>()?;
        if admitted.is_empty() {
            continue;
        }
        let _ = write!(
            place_arms,
            " WHEN {id} THEN COALESCE(array_position(ARRAY[{}], {alias}.arguments), 0)",
            admitted.join(", ")
        );
    }
    let mut keys = vec![format!("CASE {alias}.asserted_in{step_arms} ELSE 0 END")];
    if !place_arms.is_empty() {
        keys.push(format!("CASE {alias}.asserted_in{place_arms} ELSE 0 END"));
    }
    keys.push(format!("{alias}.arguments_hash"));
    Ok(keys.join(", "))
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
        let text: Option<String> = row
            .try_get(col.as_str())
            .map_err(|e| PgError::InvalidState(format!("witness column {col} missing: {e}")))?;
        // A consequent-bound variable is NULL when no row reached it: the
        // kernel's witness leaves it out too.
        let Some(text) = text else {
            continue;
        };
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
        "sum_kind" => {
            let first: bool = row
                .try_get::<Option<bool>, _>("first")
                .map_err(|e| PgError::InvalidState(format!("error column first missing: {e}")))?
                .ok_or_else(|| PgError::InvalidState("error column first is null".to_string()))?;
            let value: EvalValue = serde_json::from_str(&column("left")?).map_err(|e| {
                PgError::InvalidState(format!("error operand left undecodable: {e}"))
            })?;
            Ok(EvalError::sum_cannot_take(first, &value))
        }
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

/// One index the compiled SQL can seek on: a partial expression index
/// over one argument position of one predicate, on the digest of the
/// position's equality key. The digest keeps the entry bounded (a key
/// holds the value whole, and a subject or a collection can exceed a
/// btree entry). Correctness never depends on the index.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct IndexSpec {
    pub(crate) predicate: PredicateName,
    pub(crate) position: usize,
    /// The indexed expression, unqualified, as it appears inside the
    /// parentheses of `CREATE INDEX`.
    pub(crate) expression_sql: String,
    /// The partial-index predicate.
    pub(crate) partial_predicate_sql: String,
}

/// What the registry records the seek expression as. Named for the key
/// function, so a key with other semantics is a new label, a new digest
/// and a new index.
pub(crate) const SEEK_REPRESENTATION: &str = "value_key_v1_digest";

impl IndexSpec {
    pub(crate) fn new(predicate: PredicateName, position: usize) -> Self {
        let expression_sql = seek_expression("", position);
        let partial_predicate_sql =
            format!("predicate_name = {}", quote_literal(predicate.as_str()));
        Self {
            predicate,
            position,
            expression_sql,
            partial_predicate_sql,
        }
    }

    /// The seek expression over a query alias (`qualifier` is the alias
    /// followed by a dot), spelled as the compiled SQL spells it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn seek_expression(&self, qualifier: &str) -> String {
        seek_expression(qualifier, self.position)
    }

    /// The digest of the whole canonical specification: same digest,
    /// same physical requirement.
    pub(crate) fn digest(&self) -> String {
        use sha2::{Digest as _, Sha256};
        let canonical = format!(
            "morpholog.claims\nbtree\n{}\n{}\n{}\n{}\n{}\n",
            self.predicate,
            self.position,
            SEEK_REPRESENTATION,
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
            "morpholog_ci_{readable}_{}_vk1_{}",
            self.position,
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

/// The equality key of a position: the one equality the checks compare
/// stored values by. The module doc gives what it keys.
fn key_expression(qualifier: &str, position: usize) -> String {
    format!("morpholog.value_key_v1({qualifier}arguments -> {position})")
}

/// The digest of a position's key: what an index is built over and a
/// seek compares. The loader seeks by it too.
pub(crate) fn seek_expression(qualifier: &str, position: usize) -> String {
    format!(
        "morpholog.claim_digest({})",
        key_expression(qualifier, position)
    )
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
    /// its bindings onto the antecedent's columns. It also holds the
    /// columns a consequent binds, for the witness alone: no case is keyed
    /// by them.
    plan: ImpactPlan,
    case_cols: BTreeMap<Var, ColRef>,
    sql_select_from_where: String,
    sql_order_limit: String,
    /// The scopes the kernel could raise in, in its evaluation order,
    /// asked one at a time only after a violation row was found: the
    /// violation query stops at its first row, and an error must win over
    /// a violation that sorts earlier. The first scope that names a row
    /// wins. Empty when nothing in the body can raise.
    probe_scopes: Vec<ProbeScope>,
    /// The indexes this invariant's SQL seeks on at a join or a literal,
    /// in specification order. The plan must reach each through its
    /// index.
    pub(crate) required_indexes: Vec<IndexSpec>,
    /// The columns a case can be keyed by. The case-bound filter
    /// constrains every one, so the planner may reach a predicate through
    /// whichever it judges most selective.
    pub(crate) case_indexes: Vec<IndexSpec>,
}

/// The error query before its runtime parts: the report of the first
/// erroring binding names values in the kernel's order, which depends on
/// the transitions in flight, so the runner renders it.
/// One scope the kernel evaluates binding by binding and could raise
/// in: the prefix that reaches the raising expressions, and those
/// expressions' probes in the kernel's order.
#[derive(Debug)]
struct ProbeScope {
    from: String,
    where_: String,
    probes: Vec<Probe>,
    /// The claim aliases of the scope with their predicates, in the
    /// kernel's nesting order.
    aliases: Vec<(String, PredicateName)>,
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

    /// The error queries over the same obligation as the violation query,
    /// one per scope in the kernel's evaluation order, each ordered as the
    /// kernel evaluates: rows loaded before the transitions, then each
    /// step's admissions in statement order, alias by alias in the
    /// scope's nesting order.
    pub(crate) fn error_sqls(
        &self,
        case_filter: Option<&str>,
        steps: &[DeltaStep],
    ) -> Result<Vec<String>, PgError> {
        let order = |aliases: &[(String, PredicateName)]| -> Result<String, PgError> {
            Ok(aliases
                .iter()
                .map(|(alias, predicate)| order_keys(alias, predicate, steps))
                .collect::<Result<Vec<_>, _>>()?
                .join(", "))
        };
        self.probe_scopes
            .iter()
            .map(|scope| {
                let report = error_report(&scope.probes, &order)?;
                let mut sql = format!(
                    "SELECT {report}\nFROM {}\nWHERE {}",
                    scope.from, scope.where_
                );
                if let Some(filter) = case_filter {
                    let _ = write!(sql, "\n  AND ({filter})");
                }
                let _ = write!(sql, "\nORDER BY {}\nLIMIT 1", order(&scope.aliases)?);
                Ok(sql)
            })
            .collect()
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

/// Renders the kernel's order over a scope's aliases, given the
/// transitions in flight.
type OrderRenderer<'a> = dyn Fn(&[(String, PredicateName)]) -> Result<String, PgError> + 'a;

/// The error query's report columns: which way the first erroring row
/// raised, and what the kernel needs to word it. A sum's report looks
/// inside the sum for the first value it could not take, in the kernel's
/// order, which `order` renders; a lifted probe's report looks inside
/// its scope the same way.
fn error_report(probes: &[Probe], order: &OrderRenderer<'_>) -> Result<String, PgError> {
    let case = |column: Column| -> Result<String, PgError> {
        let mut arms = String::new();
        for e in probes {
            if let Some(v) = report_column(&e.report, column, order)? {
                let _ = write!(arms, " WHEN {} THEN {v}", e.condition);
            }
        }
        Ok(if arms.is_empty() {
            "NULL::text".to_string()
        } else {
            format!("CASE{arms} END")
        })
    };
    let kind = case(Column::Kind)?;
    let domain = case(Column::Domain)?;
    let left = case(Column::Left)?;
    let right = case(Column::Right)?;
    let first = case(Column::First)?;
    let first = if first == "NULL::text" {
        "NULL::boolean".to_string()
    } else {
        first
    };
    Ok(format!(
        "{kind} AS \"kind\",\n       {domain} AS \"domain\",\n       {left} AS \"left\",\n       {right} AS \"right\",\n       {first} AS \"first\""
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Column {
    Kind,
    Domain,
    Left,
    Right,
    /// Whether the value a sum could not take was its first: the kernel
    /// words that differently.
    First,
}

/// One report column for one way of raising, or none when that way has
/// nothing to say in it.
fn report_column(
    report: &ErrorReport,
    column: Column,
    order: &OrderRenderer<'_>,
) -> Result<Option<String>, PgError> {
    Ok(match (report, column) {
        (ErrorReport::SumRange, Column::Kind) => Some("'range'".to_string()),
        (ErrorReport::SumKind(_), Column::Kind) => Some("'sum_kind'".to_string()),
        (ErrorReport::Compare { .. }, Column::Kind) => Some("'compare'".to_string()),
        (ErrorReport::Compare { domain, .. }, Column::Domain) => Some(match domain {
            OrderedDomain::Decimal => "'decimal'".to_string(),
            OrderedDomain::Timestamp => "'timestamp'".to_string(),
            OrderedDomain::Date | OrderedDomain::Duration => unreachable!("never rendered"),
        }),
        (ErrorReport::Compare { left, .. }, Column::Left) => Some(format!("({left})::text")),
        (ErrorReport::Compare { right, .. }, Column::Right) => Some(format!("({right})::text")),
        (ErrorReport::SumKind(sum), Column::Left) => Some(format!(
            "(SELECT ({}.arguments -> {})::text FROM {} WHERE {} AND {} ORDER BY {} LIMIT 1)",
            sum.alias,
            sum.position,
            sum.from,
            sum.where_,
            sum.foreign(),
            order(&sum.aliases)?
        )),
        (ErrorReport::SumKind(sum), Column::First) => Some(format!(
            "(SELECT ({}) FROM {} WHERE {} ORDER BY {} LIMIT 1)",
            sum.foreign(),
            sum.from,
            sum.where_,
            order(&sum.aliases)?
        )),
        // A lifted probe's kind and domain are constants; its operands
        // are read from the first inner row that raises.
        (
            ErrorReport::Lifted {
                from,
                where_,
                aliases,
                condition,
                report,
            },
            column,
        ) => match report_column(report, column, order)? {
            None => None,
            Some(inner) if matches!(column, Column::Kind | Column::Domain) => Some(inner),
            Some(inner) => Some(format!(
                "(SELECT {inner} FROM {from} WHERE {} ORDER BY {} LIMIT 1)",
                and_all(&[where_.clone(), condition.clone()]),
                order(aliases)?
            )),
        },
        (ErrorReport::SumRange, Column::Domain | Column::Left | Column::Right | Column::First)
        | (ErrorReport::SumKind(_), Column::Domain | Column::Right)
        | (ErrorReport::Compare { .. }, Column::First) => None,
    })
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
    /// Every (predicate, position) the SQL seeks on, collected where the
    /// seek is emitted: the index specification.
    required: BTreeSet<(PredicateName, usize)>,
    /// Sums rendered while a comparison's operands were being rendered:
    /// the comparison collects them into its own rendering.
    pending_sums: Vec<RenderedSum>,
}

/// One sum, computed once per row of the scope it sits in as a LATERAL
/// item, with the representability test of its total and the test for a
/// value it could not take.
struct RenderedSum {
    lateral: String,
    range_error: String,
    kind_error: String,
    /// Where to look for the value the sum could not take; none for a
    /// literal target, which cannot meet one.
    kind_report: Option<SumScope>,
}

/// Where a sum's values come from, so the error query can find the first
/// one the kernel could not take.
#[derive(Debug, Clone)]
struct SumScope {
    from: String,
    where_: String,
    /// The target's column.
    alias: String,
    position: usize,
    aliases: Vec<(String, PredicateName)>,
}

impl SumScope {
    /// The target row is not the decimal the sum adds.
    fn foreign(&self) -> String {
        format!(
            "({}.arguments -> {} ->> 'type') IS DISTINCT FROM 'decimal'",
            self.alias, self.position
        )
    }
}

/// One way a scope can raise: the condition that holds on the row the
/// kernel would raise on, and what to report when it is the first such
/// row.
#[derive(Debug, Clone)]
struct Probe {
    condition: String,
    report: ErrorReport,
}

#[derive(Debug, Clone)]
enum ErrorReport {
    /// A sum's total no decimal can hold.
    SumRange,
    /// A sum met a value it cannot add.
    SumKind(SumScope),
    /// Two operands the kernel cannot order: the tagged values, as jsonb
    /// expressions, for the kernel to judge.
    Compare {
        domain: OrderedDomain,
        left: String,
        right: String,
    },
    /// A probe of a scope the kernel evaluates whole for each row of the
    /// enclosing one. Its report is read from the first inner row, in the
    /// kernel's order, that raises.
    Lifted {
        from: String,
        where_: String,
        aliases: Vec<(String, PredicateName)>,
        condition: String,
        report: Box<ErrorReport>,
    },
}

/// A scope's rendering: its joins, the LATERAL items its sums are
/// computed in, every conjunct, the scopes in it the kernel could raise
/// in, and its bindings.
#[derive(Default)]
struct Rendered {
    from: Vec<(String, PredicateName, String)>, // (alias, predicate, from item)
    laterals: Vec<String>,
    where_: Vec<String>,
    /// The scopes the kernel could raise in, in its evaluation order,
    /// each relative to this rendering: a prefix names only what this
    /// rendering joined and filtered before the raising expression.
    probes: Vec<ScopeDraft>,
    env: Env,
    /// A comparison consumed a sum. Nothing may follow it in its scope
    /// and it cannot sit under a nested scope: the sum fragment's
    /// boundary, until a forcing example moves it.
    closes_scope: bool,
    /// How many `from` items and conjuncts each conjunct of a
    /// conjunction had contributed by its end, so a later reader can
    /// take the prefix through the conjunct that bound a variable.
    stages: Vec<(usize, usize)>,
}

impl Rendered {
    fn has_errors(&self) -> bool {
        !self.probes.is_empty()
    }

    fn conjunction(&self) -> String {
        joined(&self.where_)
    }

    /// `EXISTS`-shaped rendering of this match, usable inside a WHERE.
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

    fn aliases(&self) -> Vec<(String, PredicateName)> {
        self.from
            .iter()
            .map(|(alias, predicate, _)| (alias.clone(), predicate.clone()))
            .collect()
    }
}

/// Conjuncts joined as a prefix, or `true` when there are none.
fn joined(parts: &[String]) -> String {
    if parts.is_empty() {
        "true".to_string()
    } else {
        parts.join(" AND ")
    }
}

/// A scope the kernel could raise in, relative to the rendering that
/// holds it: the prefix that reaches the raising expressions, and their
/// probes in the kernel's order.
#[derive(Debug, Clone, Default)]
struct ScopeDraft {
    from: Vec<(String, PredicateName, String)>,
    laterals: Vec<String>,
    where_: Vec<String>,
    probes: Vec<Probe>,
}

impl ScopeDraft {
    /// The enclosing rendering's prefix comes before this scope's own.
    fn prepend(&mut self, prefix: &Rendered) {
        let mut from = prefix.from.clone();
        from.append(&mut self.from);
        self.from = from;
        let mut laterals = prefix.laterals.clone();
        laterals.append(&mut self.laterals);
        self.laterals = laterals;
        let mut where_ = prefix.where_.clone();
        where_.append(&mut self.where_);
        self.where_ = where_;
    }

    fn sources(&self) -> String {
        self.from
            .iter()
            .map(|(_, _, f)| f.as_str())
            .chain(self.laterals.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn aliases(&self) -> Vec<(String, PredicateName)> {
        self.from
            .iter()
            .map(|(alias, predicate, _)| (alias.clone(), predicate.clone()))
            .collect()
    }

    /// Any of this scope's ways of raising.
    fn any(&self) -> String {
        or_all(
            &self
                .probes
                .iter()
                .map(|p| p.condition.clone())
                .collect::<Vec<_>>(),
        )
    }

    /// The scope as the error query asks it, once its prefix is whole.
    fn anchored(&self) -> ProbeScope {
        ProbeScope {
            from: self.sources(),
            where_: and_all(&[joined(&self.where_), self.any()]),
            probes: self.probes.clone(),
            aliases: self.aliases(),
        }
    }
}

/// The scopes of a proposition the kernel evaluates whole for each row
/// of the enclosing scope, collapsed into one scope of the enclosing
/// row. A filter-only inner scope is read on the row itself; one with
/// joins is asked as a correlated `EXISTS`, and its report from the
/// first inner row that raises, in the kernel's order. The result has no
/// prefix of its own: the enclosing rendering supplies it.
fn lift(drafts: &[ScopeDraft]) -> ScopeDraft {
    let mut lifted = ScopeDraft::default();
    for d in drafts {
        if d.from.is_empty() {
            lifted.laterals.extend(d.laterals.iter().cloned());
            for p in &d.probes {
                lifted.probes.push(Probe {
                    condition: and_all(&[joined(&d.where_), p.condition.clone()]),
                    report: p.report.clone(),
                });
            }
        } else {
            let from = d.sources();
            let where_ = joined(&d.where_);
            for p in &d.probes {
                lifted.probes.push(Probe {
                    condition: format!(
                        "EXISTS (SELECT 1 FROM {from} WHERE {})",
                        and_all(&[where_.clone(), p.condition.clone()])
                    ),
                    report: ErrorReport::Lifted {
                        from: from.clone(),
                        where_: where_.clone(),
                        aliases: d.aliases(),
                        condition: p.condition.clone(),
                        report: Box::new(p.report.clone()),
                    },
                });
            }
        }
    }
    lifted
}

/// The scopes of a nested proposition, lifted into one scope of the
/// enclosing row; none when nothing inside can raise.
fn lifted_scopes(drafts: &[ScopeDraft]) -> Vec<ScopeDraft> {
    if drafts.is_empty() {
        Vec::new()
    } else {
        vec![lift(drafts)]
    }
}

/// A scope's conjuncts with the ways of raising folded in at the stage
/// each arises, ending in `final_`: the WHERE items of a violation
/// query. Past the prefix a scope shares with the conjunction, a row
/// violates where that scope raises, or where the next conjunct holds
/// and the rest violates. The conjuncts before the first raising stage
/// stay separate items.
fn violated(r: &Rendered, final_: String) -> Vec<String> {
    let n = r.where_.len();
    let stage = |k: usize| -> Vec<String> {
        r.probes
            .iter()
            .filter(|d| d.where_.len() == k)
            .flat_map(|d| d.probes.iter().map(|p| p.condition.clone()))
            .collect()
    };
    let first = r.probes.iter().map(|d| d.where_.len()).min().unwrap_or(n);
    let mut rest = final_;
    for k in (first..=n).rev() {
        let conds = stage(k);
        if !conds.is_empty() {
            rest = or_all(&[or_all(&conds), rest]);
        }
        if k > first {
            rest = and_all(&[r.where_[k - 1].clone(), rest]);
        }
    }
    let mut items: Vec<String> = r.where_[..first].to_vec();
    if rest != "true" {
        items.push(rest);
    }
    items
}

/// The typed refusal for a raising scope where the fragment has no place
/// for one: a sum's, unless a comparison itself can raise.
fn shape_refusal(r: &Rendered, sum: &'static str, comparison: &'static str) -> CompileReason {
    if r.probes
        .iter()
        .flat_map(|d| d.probes.iter())
        .any(|p| matches!(p.report, ErrorReport::Compare { .. }))
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
                let (select, order) = witness_select_order(&r);
                (
                    format!(
                        "SELECT {select}\nFROM {}\nWHERE {}",
                        from_list(&r),
                        violated(&r, "true".to_string()).join("\n  AND ")
                    ),
                    format!("\nORDER BY {order}\nLIMIT 1"),
                    r.probes.iter().map(ScopeDraft::anchored).collect(),
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
    // The case-bound check seeks on the case's columns. Without their
    // indexes it walks the predicate and takes the very lock the bound
    // exists to avoid.
    let mut case_indexes: Vec<IndexSpec> = plan
        .case_variables()
        .iter()
        .filter_map(|var| case_cols.get(var))
        .map(|col| IndexSpec::new(col.predicate.clone(), col.position))
        .collect();
    case_indexes.sort();
    case_indexes.dedup();
    Ok(CompiledInvariant {
        name: inv.name.clone(),
        version: inv.version,
        witness_vars,
        plan,
        case_cols,
        sql_select_from_where: select_from_where,
        sql_order_limit: order_limit,
        probe_scopes: error,
        required_indexes: ctx
            .required
            .iter()
            .map(|(predicate, position)| IndexSpec::new(predicate.clone(), *position))
            .collect(),
        case_indexes,
    })
}

/// The dominant shape: `antecedent implies consequent`. Violation = an
/// antecedent match with no consequent match.
type Denial = (String, String, Vec<ProbeScope>, Env);

fn compile_denial(left: &Prop, right: &Prop, ctx: &mut Ctx<'_>) -> Result<Denial, CompileReason> {
    let ant = render_prop(left, Env::new(), ctx)?;
    if ant.from.is_empty() {
        // Filter-only antecedent: nothing to witness, so use the generic
        // denial.
        return generic_denial_implies(left, right, ctx);
    }
    let cons = render_prop(right, ant.env.clone(), ctx)?;
    if cons.closes_scope && !cons.from.is_empty() {
        return Err(CompileReason::SumShape {
            detail: "sum beside claim patterns in a consequent",
        });
    }
    // The kernel evaluates the whole consequent for each antecedent
    // match, so its scopes collapse into one scope of the antecedent
    // row, after the antecedent's own.
    let mut lifted = lift(&cons.probes);
    lifted.prepend(&ant);
    let not_cons = if !cons.from.is_empty() {
        format!("NOT {}", cons.exists_sql())
    } else if cons.has_errors() {
        format!("NOT {}", and_all(&cons.where_))
    } else {
        format!("NOT ({})", cons.conjunction())
    };
    let mut scope = Rendered {
        from: ant.from.clone(),
        laterals: ant.laterals.clone(),
        where_: ant.where_.clone(),
        probes: ant.probes.clone(),
        ..Rendered::default()
    };
    scope.laterals.extend(lifted.laterals.iter().cloned());
    if !lifted.probes.is_empty() {
        scope.probes.push(lifted);
    }
    let (mut select, order) = witness_select_order(&ant);
    // The kernel's witness carries the variables of the longest
    // consequent prefix that still matched, bound on its first row.
    // Each such variable is read from the first row of the prefix
    // through the conjunct that binds it, NULL when none exists.
    let bound_by_consequent: Vec<&Var> = cons
        .env
        .keys()
        .filter(|v| !ant.env.contains_key(*v))
        .collect();
    for var in &bound_by_consequent {
        let col = &cons.env[*var];
        let Some(alias_index) = cons
            .from
            .iter()
            .position(|(alias, _, _)| *alias == col.alias)
        else {
            continue;
        };
        let (from_len, where_len) = cons
            .stages
            .iter()
            .copied()
            .find(|(from_len, _)| *from_len > alias_index)
            .unwrap_or((cons.from.len(), cons.where_.len()));
        let prefix = Rendered {
            from: cons.from[..from_len].to_vec(),
            where_: cons.where_[..where_len].to_vec(),
            ..Rendered::default()
        };
        let _ = write!(
            select,
            ",\n       (SELECT ({}.arguments -> {})::text FROM {} WHERE {} ORDER BY {} LIMIT 1) AS {}",
            col.alias,
            col.position,
            from_list(&prefix),
            prefix.conjunction(),
            prefix
                .from
                .iter()
                .map(|(alias, _, _)| format!("{alias}.arguments_hash"))
                .collect::<Vec<_>>()
                .join(", "),
            quote_ident(&format!("w_{var}"))
        );
    }
    let mut case_cols = ant.env.clone();
    for var in bound_by_consequent {
        case_cols.insert(var.clone(), cons.env[var].clone());
    }
    let error = scope.probes.iter().map(ScopeDraft::anchored).collect();
    Ok((
        format!(
            "SELECT {select}\nFROM {}\nWHERE {}",
            from_list(&scope),
            violated(&scope, not_cons).join("\n  AND ")
        ),
        format!("\nORDER BY {order}\nLIMIT 1"),
        error,
        case_cols,
    ))
}

/// Any other top-level shape: the invariant holds iff the body matches at
/// all, so violation is bare non-existence, with an empty witness.
fn generic_denial(body: &Prop, ctx: &mut Ctx<'_>) -> Result<Denial, CompileReason> {
    let r = render_prop(body, Env::new(), ctx)?;
    if !r.has_errors() {
        return Ok((
            format!("SELECT 1 AS \"w\"\nWHERE NOT {}", r.exists_sql()),
            String::new(),
            Vec::new(),
            Env::new(),
        ));
    }
    // The kernel evaluates every binding of every scope, so an error
    // anywhere dominates; otherwise the body must match somewhere.
    let raises = or_all(
        &r.probes
            .iter()
            .map(|d| {
                format!(
                    "EXISTS (SELECT 1 FROM {} WHERE {})",
                    d.sources(),
                    and_all(&[joined(&d.where_), d.any()])
                )
            })
            .collect::<Vec<_>>(),
    );
    let holds = format!(
        "EXISTS (SELECT 1 FROM {} WHERE {})",
        from_list(&r),
        and_all(&r.where_)
    );
    Ok((
        format!("SELECT 1 AS \"w\"\nWHERE {raises} OR NOT {holds}"),
        String::new(),
        r.probes.iter().map(ScopeDraft::anchored).collect(),
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
    if l.closes_scope || r.closes_scope {
        return Err(CompileReason::SumShape {
            detail: "sum in a filter-only implication",
        });
    }
    let not_r = if r.from.is_empty() {
        format!("NOT ({})", r.conjunction())
    } else {
        format!("NOT {}", r.exists_sql())
    };
    let mut lifted = lift(&r.probes);
    lifted.prepend(&l);
    let mut scope = Rendered {
        laterals: l.laterals.clone(),
        where_: l.where_.clone(),
        probes: l.probes.clone(),
        ..Rendered::default()
    };
    scope.laterals.extend(lifted.laterals.iter().cloned());
    if !lifted.probes.is_empty() {
        scope.probes.push(lifted);
    }
    let error = scope.probes.iter().map(ScopeDraft::anchored).collect();
    let items = violated(&scope, not_r);
    let where_ = if scope.laterals.is_empty() {
        format!("({})", items.join(" AND "))
    } else {
        format!(
            "EXISTS (SELECT 1 FROM {} WHERE {})",
            from_list(&scope),
            items.join(" AND ")
        )
    };
    Ok((
        format!("SELECT 1 AS \"w\"\nWHERE {where_}"),
        String::new(),
        error,
        Env::new(),
    ))
}

fn from_list(r: &Rendered) -> String {
    r.from
        .iter()
        .map(|(_, _, f)| f.as_str())
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
    // Order by the keys, not raw `arguments`. Raw order matches the
    // primary key, which tempts the planner into an early-stop scan of
    // the whole predicate (seen at 100k rows). Deterministic in everything
    // the row reports.
    let order = if r.env.is_empty() {
        r.from
            .iter()
            .map(|(alias, _, _)| format!("{alias}.arguments"))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        r.env
            .values()
            .map(|col| format!("({})::text", key_sql(col)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    (select, order)
}

/// A position's equality key in a query.
fn key_sql(col: &ColRef) -> String {
    key_expression(&format!("{}.", col.alias), col.position)
}

/// A position's seek: the digest of its key, what the index holds.
fn seek_sql(col: &ColRef) -> String {
    seek_expression(&format!("{}.", col.alias), col.position)
}

/// The key of a runtime value, as a constant the planner folds: the
/// value's own serialisation through the same function the rows go
/// through, so there is one keying and no second implementation.
fn key_lit(ev: &EvalValue) -> Result<String, CompileReason> {
    Ok(format!(
        "morpholog.value_key_v1({}::jsonb)",
        quote_literal(&tagged_json(ev)?)
    ))
}

/// Two positions hold kernel-equal values, by the digests of their keys:
/// what the index holds, so the inner side of a join comes back from the
/// index without recomputing anything. Digest equality is the claims
/// table's own identity (its primary key is the digest of the arguments),
/// so no weaker an assumption is made here than there. An exact key
/// beside the digest was measured: on a rule that sums a group for every
/// row of the group, recomputing two keys per inner row cost five times
/// the check.
fn col_eq(a: &ColRef, b: &ColRef) -> String {
    format!("({}) = ({})", seek_sql(a), seek_sql(b))
}

/// A position holds exactly this runtime value, by the digest of its key.
fn const_eq_sql(col: &ColRef, ev: &EvalValue) -> Result<String, CompileReason> {
    Ok(format!(
        "({}) = morpholog.claim_digest({})",
        seek_sql(col),
        key_lit(ev)?
    ))
}

/// The whole tagged value at a position, jsonb.
fn tagged_sql(col: &ColRef) -> String {
    format!("{}.arguments -> {}", col.alias, col.position)
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

/// A decimal position's amount as numeric, NULL for a value of another
/// kind: a guarded read, since the planner may evaluate it on any row.
fn decimal_sql(col: &ColRef) -> String {
    format!(
        "(CASE WHEN ({}.arguments -> {} ->> 'type') = 'decimal' THEN ({}.arguments -> {} ->> 'value')::numeric END)",
        col.alias, col.position, col.alias, col.position
    )
}

/// A quantity position's amount as numeric, NULL for another kind.
fn amount_sql(col: &ColRef) -> String {
    format!(
        "(CASE WHEN ({}.arguments -> {} ->> 'type') = 'quantity' THEN ({}.arguments -> {} -> 'value' ->> 'amount')::numeric END)",
        col.alias, col.position, col.alias, col.position
    )
}

/// A quantity position's unit, text; NULL for any other stored kind.
fn unit_sql(col: &ColRef) -> String {
    format!(
        "{}.arguments -> {} -> 'value' ->> 'unit'",
        col.alias, col.position
    )
}

/// A literal in a claim pattern: the position must hold exactly it.
fn literal_filter(col: &ColRef, value: &Value, ctx: &mut Ctx<'_>) -> Result<String, CompileReason> {
    if matches!(value, Value::CalendarSpan(_)) {
        return Err(CompileReason::Literal {
            kind: "calendar span",
        });
    }
    let ev = literal_eval(value)?;
    ctx.required.insert((col.predicate.clone(), col.position));
    const_eq_sql(col, &ev)
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
/// value cannot be stored at all (widens to Unbounded).
fn const_eq(col: &ColRef, ev: &EvalValue) -> Option<String> {
    fn storable(ev: &EvalValue) -> bool {
        match ev {
            EvalValue::CalendarSpan(_) => false,
            EvalValue::Collection(items) => items.iter().all(storable),
            _ => true,
        }
    }
    if !storable(ev) {
        return None;
    }
    const_eq_sql(col, ev).ok()
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
                if acc.closes_scope {
                    return Err(CompileReason::SumShape {
                        detail: "a sum comparison must be the last conjunct of its scope",
                    });
                }
                let mut r = render_prop(p, acc.env.clone(), ctx)?;
                // A violation query returns a row of the whole
                // conjunction, so a binding that raises before a join and
                // has no row past it would go unreported.
                if acc.has_errors() && !r.from.is_empty() {
                    return Err(shape_refusal(
                        &acc,
                        "a claim pattern after a sum comparison in its scope",
                        "a claim pattern after an ordering in its scope",
                    ));
                }
                for d in &mut r.probes {
                    d.prepend(&acc);
                }
                acc.from.extend(r.from);
                acc.laterals.extend(r.laterals);
                acc.where_.extend(r.where_);
                acc.probes.extend(r.probes);
                acc.env = r.env;
                acc.closes_scope = r.closes_scope;
                acc.stages.push((acc.from.len(), acc.where_.len()));
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
                probes: lifted_scopes(&r.probes),
                env,
                ..Rendered::default()
            })
        }
        Prop::Exists { binding: _, body } => {
            let r = nested_scope(render_prop(body, env.clone(), ctx)?)?;
            Ok(Rendered {
                where_: vec![r.exists_sql()],
                probes: lifted_scopes(&r.probes),
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
            let mut where_ = l.where_.clone();
            where_.push(not_r);
            let violated = Rendered {
                from: l.from.clone(),
                where_,
                ..Rendered::default()
            };
            // The kernel evaluates the whole antecedent, then the whole
            // consequent for each of its matches, all for each enclosing
            // row.
            let mut drafts = l.probes.clone();
            for d in &r.probes {
                let mut d = d.clone();
                d.prepend(&l);
                drafts.push(d);
            }
            Ok(Rendered {
                where_: vec![format!("NOT {}", violated.exists_sql())],
                probes: lifted_scopes(&drafts),
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
                    where_.push(col_eq(bound, &col));
                    ctx.required
                        .insert((bound.predicate.clone(), bound.position));
                    ctx.required.insert((predicate.clone(), i));
                } else {
                    env.insert(v.clone(), col);
                }
            }
        }
    }
    Ok(Rendered {
        from: vec![(
            alias.clone(),
            predicate.clone(),
            format!("morpholog.claims {alias}"),
        )],
        where_,
        env,
        ..Rendered::default()
    })
}

/// A scope checked by existence may stop at its first match, while a sum
/// is computed for every binding of the scope it closes, so no
/// sum-closing rendering compiles under one.
fn nested_scope(r: Rendered) -> Result<Rendered, CompileReason> {
    if r.closes_scope {
        return Err(CompileReason::SumShape {
            detail: "sum under a nested scope",
        });
    }
    Ok(r)
}

/// A value in comparison position: its key when it has one (a stored
/// value or a literal; a sum's total has none), and how it orders.
struct Operand {
    key: Option<String>,
    ordered: Ordered,
}

/// How a value takes part in an ordered comparison, with the test that
/// it is of the kind the comparison expects and its tagged form for the
/// kernel to word an error with. `well_typed` is `true` for a literal or
/// a total.
enum Ordered {
    Numeric {
        sql: String,
        well_typed: String,
        tagged: String,
    },
    Quantity {
        amount: String,
        unit: String,
        well_typed: String,
        tagged: String,
        /// The unit is a literal, so two literals never raise.
        literal: bool,
    },
    Timestamp {
        nanos: String,
        well_typed: String,
        tagged: String,
    },
    /// A kind nothing orders here, or one the compiler cannot tell at
    /// compile time: the declared kind, for a column.
    None(Option<PredicateArgKind>),
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
    let clause = match (&l.key, &r.key, &l.ordered, &r.ordered) {
        (Some(x), Some(y), _, _) => format!("({x}) = ({y})"),
        // Two totals: decimals, never null.
        (None, None, Ordered::Numeric { sql: x, .. }, Ordered::Numeric { sql: y, .. }) => {
            format!("({x}) = ({y})")
        }
        // A total against a stored value: equal only to a decimal of the
        // same amount, and a value of another kind reads as null here.
        (None, _, Ordered::Numeric { sql: x, .. }, Ordered::Numeric { sql: y, .. })
        | (_, None, Ordered::Numeric { sql: x, .. }, Ordered::Numeric { sql: y, .. }) => {
            format!("({x}) IS NOT DISTINCT FROM ({y})")
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

/// An ordered comparison. Where the kernel raises (an operand of another
/// kind, two quantities of two units), the SQL carries it as data.
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
    let (clause, raises) = match (domain, &l.ordered, &r.ordered) {
        (
            OrderedDomain::Decimal,
            Ordered::Numeric {
                sql: x,
                well_typed: xw,
                tagged: xt,
            },
            Ordered::Numeric {
                sql: y,
                well_typed: yw,
                tagged: yt,
            },
        ) => (
            format!("({x}) {op} ({y})"),
            raising(and_all(&[xw.clone(), yw.clone()]), domain, xt, yt),
        ),
        (
            OrderedDomain::Decimal,
            Ordered::Quantity {
                amount: xa,
                unit: xu,
                well_typed: xw,
                tagged: xt,
                literal: xl,
            },
            Ordered::Quantity {
                amount: ya,
                unit: yu,
                well_typed: yw,
                tagged: yt,
                literal: yl,
            },
        ) => (
            format!("({xa}) {op} ({ya})"),
            // Two literals of one unit are checked at authoring time.
            if *xl && *yl {
                None
            } else {
                raising(
                    and_all(&[xw.clone(), yw.clone(), format!("({xu}) = ({yu})")]),
                    domain,
                    xt,
                    yt,
                )
            },
        ),
        (
            OrderedDomain::Timestamp,
            Ordered::Timestamp {
                nanos: xn,
                well_typed: xw,
                tagged: xt,
            },
            Ordered::Timestamp {
                nanos: yn,
                well_typed: yw,
                tagged: yt,
            },
        ) => (
            format!("({xn}) {op} ({yn})"),
            raising(and_all(&[xw.clone(), yw.clone()]), domain, xt, yt),
        ),
        (_, Ordered::None(Some(kind)), _) | (_, _, Ordered::None(Some(kind))) => {
            // A position of a kind the SQL cannot order by at compile
            // time (`Any`, a collection), which the checker admits.
            return Err(CompileReason::ArgumentKind { kind: kind.clone() });
        }
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
fn raising(well_typed: String, domain: OrderedDomain, left: &str, right: &str) -> Option<Probe> {
    if well_typed == "true" {
        return None;
    }
    Some(Probe {
        condition: format!("NOT {well_typed}"),
        report: ErrorReport::Compare {
            domain,
            left: left.to_string(),
            right: right.to_string(),
        },
    })
}

/// A comparison's rendering: its clause as a conjunct, and, where
/// anything can raise, one scope holding its sums' probes ahead of its
/// own, as the kernel meets them. A comparison over a sum closes its
/// scope.
fn close_comparison(
    clause: String,
    raises: Option<Probe>,
    env: &Env,
    ctx: &mut Ctx<'_>,
) -> Rendered {
    let sums = std::mem::take(&mut ctx.pending_sums);
    let mut probes: Vec<Probe> = Vec::new();
    for s in &sums {
        if let Some(scope) = &s.kind_report {
            probes.push(Probe {
                condition: s.kind_error.clone(),
                report: ErrorReport::SumKind(scope.clone()),
            });
        }
        probes.push(Probe {
            condition: s.range_error.clone(),
            report: ErrorReport::SumRange,
        });
    }
    probes.extend(raises);
    if probes.is_empty() {
        return Rendered {
            where_: vec![clause],
            env: env.clone(),
            ..Rendered::default()
        };
    }
    let laterals: Vec<String> = sums.into_iter().map(|s| s.lateral).collect();
    Rendered {
        laterals: laterals.clone(),
        where_: vec![clause],
        closes_scope: !laterals.is_empty(),
        probes: vec![ScopeDraft {
            laterals,
            probes,
            ..ScopeDraft::default()
        }],
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
            let ordered = match &col.kind {
                PredicateArgKind::Decimal => Ordered::Numeric {
                    sql: decimal_sql(col),
                    well_typed: tagged_as_sql(col, "decimal"),
                    tagged: tagged_sql(col),
                },
                PredicateArgKind::Quantity(_) => Ordered::Quantity {
                    amount: amount_sql(col),
                    unit: unit_sql(col),
                    well_typed: tagged_as_sql(col, "quantity"),
                    tagged: tagged_sql(col),
                    literal: false,
                },
                PredicateArgKind::Timestamp => Ordered::Timestamp {
                    nanos: format!("morpholog.timestamp_nanos({})", tagged_sql(col)),
                    well_typed: tagged_as_sql(col, "timestamp"),
                    tagged: tagged_sql(col),
                },
                other => Ordered::None(Some(other.clone())),
            };
            Ok(Operand {
                key: Some(key_sql(col)),
                ordered,
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
            let (val, kind_test, kind_report): (String, String, Option<SumScope>) =
                match value.as_ref() {
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
                        let scope = SumScope {
                            from: from_list(&r),
                            where_: r.conjunction(),
                            alias: col.alias.clone(),
                            position: col.position,
                            aliases: r.aliases(),
                        };
                        (
                            decimal_sql(col),
                            format!("bool_or({})", scope.foreign()),
                            Some(scope),
                        )
                    }
                    ValueExpr::Term(Term::Literal(Value::Decimal(d))) => (
                        format!("{}::numeric", quote_literal(d)),
                        "false".to_string(),
                        None,
                    ),
                    _ => {
                        return Err(CompileReason::SumShape {
                            detail: "computed target",
                        });
                    }
                };
            // Computed once per row of the enclosing scope, so the
            // comparison, the representability test and the kind test
            // read one pass.
            let alias = format!("l{}", ctx.counter);
            ctx.counter += 1;
            let total = format!("{alias}.s");
            ctx.pending_sums.push(RenderedSum {
                lateral: format!(
                    "LATERAL (SELECT COALESCE(sum({val}), 0::numeric) AS s, COALESCE({kind_test}, false) AS f FROM {} WHERE {}) {alias}",
                    from_list(&r),
                    r.conjunction()
                ),
                range_error: range_error_sql(&total),
                kind_error: format!("{alias}.f"),
                kind_report,
            });
            Ok(Operand {
                key: None,
                ordered: Ordered::Numeric {
                    sql: total.clone(),
                    well_typed: "true".to_string(),
                    tagged: format!(
                        "jsonb_build_object('type', 'decimal', 'value', ({total})::text)"
                    ),
                },
            })
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

/// A literal in value position: its key, and a SQL constant of the
/// kernel's value for ordering. A timestamp's coordinate is computed here,
/// not by the database.
fn literal_operand(value: &Value) -> Result<Operand, CompileReason> {
    if matches!(value, Value::CalendarSpan(_)) {
        return Err(CompileReason::Literal {
            kind: "calendar span",
        });
    }
    let ev = literal_eval(value)?;
    let tagged = format!("{}::jsonb", quote_literal(&tagged_json(&ev)?));
    let ordered = match &ev {
        EvalValue::Decimal(d) => Ordered::Numeric {
            sql: format!("{}::numeric", quote_literal(&d.to_string())),
            well_typed: "true".to_string(),
            tagged: tagged.clone(),
        },
        EvalValue::Quantity { amount, unit } => Ordered::Quantity {
            amount: format!("{}::numeric", quote_literal(&amount.to_string())),
            unit: quote_literal(unit.as_str()),
            well_typed: "true".to_string(),
            tagged: tagged.clone(),
            literal: true,
        },
        EvalValue::Timestamp(t) => Ordered::Timestamp {
            nanos: format!("{}::numeric", t.as_nanosecond()),
            well_typed: "true".to_string(),
            tagged: tagged.clone(),
        },
        _ => Ordered::None(None),
    };
    Ok(Operand {
        key: Some(key_lit(&ev)?),
        ordered,
    })
}

#[cfg(test)]
mod tests;
