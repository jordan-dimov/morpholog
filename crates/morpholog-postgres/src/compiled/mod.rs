//! Compile fragment invariants into SQL violation queries.
//!
//! Denial orientation: each invariant compiles to one query that returns a
//! witnessing row when the invariant FAILS over the claims table and no rows
//! when it holds. The kernel semantics being reproduced: an invariant holds
//! iff its body yields at least one binding witness; `Implies`/`Forall`/
//! `Exists`/`Not` export no bindings; `And` threads bindings left to right;
//! decimal comparison is scale-insensitive (`::numeric`, never JSON text).
//!
//! The compiler is also the fragment classifier: an invariant outside the
//! fragment is a [`CompileRefusal`], collected whole-run like
//! `sql_views::ViewRefusal`. Input is a [`ValidatedProgram`], so a refusal
//! means exactly "a valid Morpholog invariant the fragment cannot express" -
//! undeclared names and arity mismatches stay validation errors.
//!
//! Equality representations are exhaustive per declared kind, each with its
//! proof, never a convenience fallback:
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
//! - `Quantity` (decimal-string amount), `Collection` (may hold decimals),
//!   `Any` (may hold anything): no equality representation is proved, so a
//!   variable join or filter on such a position refuses by kind.
//!
//! The kernel stays the executable spec: the compiled path's correctness
//! claim is held by the same-candidate differential in
//! `compiled_differential`, which stages each probe's delta once and
//! requires the kernel and both compiled stages to agree over it.
//!
//! The adopted witness contract (spike verdict): rule name, version, and
//! the witness VARIABLE SET are strict across evaluators; witness values
//! are observational - a symmetric self-join lawfully names the violating
//! pair in a different order. (The spike had a second cause, bodies
//! minting `new Subject()` twice; the same-candidate differential stages
//! the body once, so that source of divergence no longer exists.)

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use rust_decimal::Decimal;

use morpholog_core::{
    ClaimInstance, EvalError, EvalValue, Invariant, InvariantName, OrderedDomain, PredicateArgKind,
    PredicateDecl, PredicateName, Prop, SumSeed, Term, ValidatedProgram, Value, ValueExpr, Var,
    WitnessBinding,
};
use sqlx::{Postgres, Row, Transaction};

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

/// Every invariant of a programme, compiled. Programme order is preserved:
/// the runner evaluates in this order and refuses on the first violation,
/// matching the kernel's first-failure contract.
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

/// Which check runs: the whole stage-1 query, or stage 2 bounded to the
/// cases the delta could have changed. Production runs stage 1; stage 2
/// stays differential-proven until its audit semantics are decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Full,
    /// Dormant in production until its audit semantics are decided;
    /// the differential keeps it proven.
    #[cfg_attr(not(test), allow(dead_code))]
    CaseBound,
}

/// A violation the runner found: the rule, its version, its witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SqlViolation {
    pub(crate) name: InvariantName,
    pub(crate) version: u32,
    pub(crate) witness: Vec<WitnessBinding>,
}

/// Correlated-subquery estimates inflate planned cost past the JIT
/// threshold (~118ms of compilation for a sub-ms plan, measured at
/// 100k claims in the spike). Off for the rest of this transaction;
/// JIT is for analytics.
pub(crate) async fn disable_jit(tx: &mut Transaction<'_, Postgres>) -> Result<(), PgError> {
    sqlx::raw_sql("SET LOCAL jit = off")
        .execute(&mut **tx)
        .await
        .map_err(classify)?;
    Ok(())
}

impl CompiledInvariantSet {
    /// The first violating invariant in programme order with its decoded
    /// witness, or `None` when every check holds: the compiled analogue
    /// of the kernel's first-failure loop. Runs inside the caller's
    /// transaction, over the claims table as the written delta left it.
    /// The commit path and the differential both run checks through
    /// here, so the differential proves the runner that admits
    /// transitions.
    pub(crate) async fn first_violation(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        stage: Stage,
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
    ) -> Result<Option<SqlViolation>, PgError> {
        for inv in &self.invariants {
            let sql = match stage {
                Stage::Full => inv.violation_sql(None),
                Stage::CaseBound => match inv.case_filter(asserted, retracted) {
                    CaseFilter::Untouched => continue,
                    CaseFilter::Bounded(filter) => inv.violation_sql(Some(&filter)),
                    CaseFilter::Unbounded => inv.violation_sql(None),
                },
            };
            // Audited for AssertSqlSafe: the SQL is rendered entirely by
            // this module from a validated programme - identifiers are
            // quoted, literals escaped, and the provenance comment
            // neutralised.
            let row = sqlx::query(sqlx::AssertSqlSafe(sql))
                .fetch_optional(&mut **tx)
                .await
                .map_err(classify)?;
            if let Some(row) = row {
                let range_error: bool = row.try_get("range_error").map_err(|e| {
                    PgError::InvalidState(format!("range_error column missing: {e}"))
                })?;
                if range_error {
                    return Err(PgError::Kernel(EvalError::sum_out_of_decimal_range()));
                }
                if let Some(range_sql) = &inv.sql_range {
                    let any = sqlx::query(sqlx::AssertSqlSafe(range_sql.clone()))
                        .fetch_optional(&mut **tx)
                        .await
                        .map_err(classify)?;
                    if any.is_some() {
                        return Err(PgError::Kernel(EvalError::sum_out_of_decimal_range()));
                    }
                }
                return Ok(Some(SqlViolation {
                    name: inv.name.clone(),
                    version: inv.version,
                    witness: decode_witness(inv, &row)?,
                }));
            }
        }
        Ok(None)
    }
}

/// Decode a violation row's witness columns: each is the `::text` of
/// the full tagged value, so `EvalValue`'s own serde is the decoder -
/// the one wire contract, no per-kind column logic.
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

/// One index the compiled SQL can seek on: a partial expression index
/// over one argument position of one predicate, in the representation
/// the SQL reads that position with. Emitted by the compiler beside the
/// query, from the same [`Representation`], so the index necessarily
/// matches the extractor. Provisioning reconciles these against the
/// database; correctness never depends on them.
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

    /// The deterministic name in Morpholog's reserved namespace: a
    /// readable prefix and the digest that makes it unique, well under
    /// the identifier limit.
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
// The checks themselves are dormant until the stage-1 integration
// reaches production; the differential exercises them under test.
pub(crate) enum CaseFilter {
    /// Delta disjoint from the invariant's occurrences: skip it entirely.
    Untouched,
    /// The touched cases, as a SQL disjunction over the antecedent's
    /// columns - spliced into the stage-1 query.
    Bounded(String),
    /// Touched, but not boundable to antecedent columns: run full stage 1.
    Unbounded,
}

#[derive(Debug, Clone)]
struct ColRef {
    alias: String,
    predicate: PredicateName,
    position: usize,
    kind: PredicateArgKind,
}

/// A claim pattern occurring anywhere in the body: which delta claims can
/// affect this invariant, and how their constants bound the antecedent.
#[derive(Debug, Clone)]
struct OccurrenceBinder {
    predicate: PredicateName,
    /// Literal guards: a delta claim mismatching one cannot affect this
    /// occurrence.
    guards: Vec<(usize, Value)>,
    /// Occurrence position -> antecedent variable (only vars the witness
    /// columns carry; others merely widen the case).
    var_map: Vec<(usize, Var)>,
}

#[derive(Debug)]
pub(crate) struct CompiledInvariant {
    pub(crate) name: InvariantName,
    pub(crate) version: u32,
    /// Witness variables, sorted by name. Each violation row carries the
    /// full tagged value as `w_<var>`, decoded through `EvalValue`'s own
    /// serde - the one wire contract, no second kind decoder.
    pub(crate) witness_vars: Vec<Var>,
    occurrences: Vec<OccurrenceBinder>,
    case_cols: BTreeMap<Var, ColRef>,
    sql_select_from_where: String,
    sql_order_limit: String,
    /// Whether any row in the invariant's scope has an unrepresentable
    /// sum, asked only after the violation query returned a violation:
    /// that query stops at its first row in witness order, and the
    /// range error must dominate a violation that merely sorts earlier.
    /// `None` when the invariant has no sum, or its violation query
    /// already answers over the whole scope.
    sql_range: Option<String>,
    /// The indexes this invariant's SQL can seek on, in specification
    /// order.
    pub(crate) required_indexes: Vec<IndexSpec>,
}

impl CompiledInvariant {
    /// The violation query. `case_filter` is a stage-2 bound produced by
    /// [`Self::case_filter`]; `None` is the full stage-1 check.
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

    #[cfg(test)]
    pub(crate) fn range_sql(&self) -> Option<&str> {
        self.sql_range.as_deref()
    }

    /// Bound the check to the cases a delta could have changed. Sound by
    /// widening: a binder that cannot constrain a variable widens toward
    /// full stage 1, never narrows past a touched case.
    pub(crate) fn case_filter(
        &self,
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
    ) -> CaseFilter {
        let mut disjuncts: BTreeSet<String> = BTreeSet::new();
        let mut touched = false;
        for claim in asserted.iter().chain(retracted) {
            for occ in &self.occurrences {
                if occ.predicate != claim.predicate {
                    continue;
                }
                if !occ.guards.iter().all(|(pos, lit)| {
                    claim
                        .args
                        .get(*pos)
                        .is_some_and(|ev| literal_matches(lit, ev))
                }) {
                    continue;
                }
                touched = true;
                if occ.var_map.is_empty() {
                    return CaseFilter::Unbounded;
                }
                let mut parts = Vec::new();
                for (pos, var) in &occ.var_map {
                    let col = &self.case_cols[var];
                    let Some(ev) = claim.args.get(*pos) else {
                        return CaseFilter::Unbounded;
                    };
                    match const_eq(col, ev) {
                        Some(sql) => parts.push(sql),
                        None => return CaseFilter::Unbounded,
                    }
                }
                disjuncts.insert(parts.join(" AND "));
            }
        }
        if !touched {
            return CaseFilter::Untouched;
        }
        let filter = disjuncts.into_iter().collect::<Vec<_>>().join(") OR (");
        CaseFilter::Bounded(format!("({filter})"))
    }
}

/// An invariant name is an opaque string in hand-built IR; neutralise
/// every sequence that could break the block comment it travels in.
/// PostgreSQL block comments NEST, so an embedded `/*` is as hostile
/// as `*/`: it opens a level our single closer would then close,
/// leaving the real comment open over the rest of the statement.
fn comment_safe(name: &str) -> String {
    name.replace(['\r', '\n'], " ")
        .replace("*/", "* /")
        .replace("/*", "/ *")
}

/// Compile every invariant of a validated programme, or report every
/// refusal (whole-run: nothing compiles unless everything does).
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

/// (predicate, literal guards, var positions) as collected during the
/// walk, before restriction to the antecedent's columns.
type RawOccurrence = (PredicateName, Vec<(usize, Value)>, Vec<(usize, Var)>);

struct Ctx<'a> {
    decls: &'a BTreeMap<&'a str, &'a PredicateDecl>,
    counter: usize,
    occurrences: Vec<RawOccurrence>,
    /// Every (predicate, position, representation) the rendered SQL
    /// filters or joins on - the index specification, collected where
    /// the extractor is emitted so both come from the same object.
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

/// A scope's rendering. `where_` holds the conjuncts before any sum
/// comparison; `tail` is that comparison, the last conjunct of its
/// scope, with the sums it reads in `laterals` and each total's
/// representability in `range_errors`. Keeping the tail apart is what
/// lets a violation query put the range error where the kernel
/// evaluates the sum: reached only past the prefix, dominating the
/// comparison once reached.
#[derive(Default)]
struct Rendered {
    from: Vec<(String, String)>, // (alias, from item)
    laterals: Vec<String>,
    where_: Vec<String>,
    tail: Option<String>,
    range_errors: Vec<String>,
    env: Env,
}

impl Rendered {
    fn has_sum(&self) -> bool {
        !self.range_errors.is_empty()
    }

    /// The prefix conjuncts, or `true` when there are none.
    fn prefix(&self) -> String {
        if self.where_.is_empty() {
            "true".to_string()
        } else {
            self.where_.join(" AND ")
        }
    }

    /// Any of this scope's sums unrepresentable, or `false`.
    fn range_error(&self) -> String {
        if self.range_errors.is_empty() {
            "false".to_string()
        } else {
            format!("({})", self.range_errors.join(" OR "))
        }
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
    /// Never reached with a sum in scope: those shapes are refused
    /// before rendering nests them.
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

/// The total is a representable decimal iff, normalised, its scale is
/// at most 28 and its coefficient fits 96 bits - the kernel's own
/// test on its wide accumulator. Data, never a thrown error, so the
/// planner's evaluation order cannot change what the query reports.
/// Conjunction without the `true` an absent part contributes.
fn and_all(parts: &[String]) -> String {
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

/// Disjunction without the `false` an absent part contributes.
fn or_all(parts: &[String]) -> String {
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
        occurrences: Vec::new(),
        required: BTreeSet::new(),
        pending_sums: Vec::new(),
    };

    let (select_from_where, order_limit, sql_range, case_cols) = match &inv.body {
        Prop::Implies { left, right } => compile_denial(left, right, &mut ctx)?,
        Prop::Forall {
            binding: _,
            source,
            body,
        } => compile_denial(source, body, &mut ctx)?,
        // Top-level Not: violated iff the inner matches. The inner's
        // bindings bound the case for stage 2, but the kernel reports
        // no witness for a failure with nothing bound above it, so
        // neither does the check.
        Prop::Not(inner) => {
            let r = render_prop(inner, Env::new(), &mut ctx)?;
            if r.from.is_empty() {
                generic_denial(&inv.body, &mut ctx)?
            } else {
                // Violated iff the inner matches: past the prefix, a
                // range error or a holding tail.
                let (select, order) = witness_select_order(&r);
                let violated = or_all(&[
                    r.range_error(),
                    r.tail.clone().unwrap_or_else(|| "true".to_string()),
                ]);
                let mut where_ = r.where_.clone();
                if violated != "true" {
                    where_.push(violated);
                }
                (
                    format!(
                        "SELECT {select},\n       {} AS \"range_error\"\nFROM {}\nWHERE {}",
                        r.range_error(),
                        from_list(&r),
                        where_.join("\n  AND ")
                    ),
                    format!("\nORDER BY {order}\nLIMIT 1"),
                    range_query(&r),
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
    let occurrences = ctx
        .occurrences
        .into_iter()
        .map(|(predicate, guards, var_map)| OccurrenceBinder {
            predicate,
            guards,
            var_map: var_map
                .into_iter()
                .filter(|(_, v)| case_cols.contains_key(v))
                .collect(),
        })
        .collect();

    Ok(CompiledInvariant {
        name: inv.name.clone(),
        version: inv.version,
        witness_vars,
        occurrences,
        case_cols,
        sql_select_from_where: select_from_where,
        sql_order_limit: order_limit,
        sql_range,
        required_indexes: ctx
            .required
            .iter()
            .map(|(predicate, position, repr)| IndexSpec::new(predicate.clone(), *position, *repr))
            .collect(),
    })
}

/// The dominant shape: `antecedent implies consequent`. Violation = an
/// antecedent match with no consequent match.
type Denial = (String, String, Option<String>, Env);

fn compile_denial(left: &Prop, right: &Prop, ctx: &mut Ctx<'_>) -> Result<Denial, CompileReason> {
    let ant = render_prop(left, Env::new(), ctx)?;
    if ant.from.is_empty() {
        // Filter-only antecedent: no generators to witness; use the
        // generic whole-body denial.
        return generic_denial_implies(left, right, ctx);
    }
    let cons = render_prop(right, ant.env.clone(), ctx)?;
    if cons.has_sum() && !cons.from.is_empty() {
        return Err(CompileReason::SumShape {
            detail: "sum beside claim patterns in a consequent",
        });
    }
    // Reached past the antecedent's prefix: the antecedent's own range
    // error, or its tail holding and the consequent failing. A
    // consequent's range error counts only once its prefix holds; a
    // failing prefix is an ordinary violation that never reaches the
    // sum.
    let ant_tail = ant.tail.clone().unwrap_or_else(|| "true".to_string());
    let cons_range = if cons.has_sum() {
        and_all(&[cons.prefix(), cons.range_error()])
    } else {
        "false".to_string()
    };
    let not_cons = if cons.has_sum() {
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
    let range_error = or_all(&[ant.range_error(), and_all(&[ant_tail.clone(), cons_range])]);
    let (select, order) = witness_select_order(&ant);
    let mut where_ = ant.where_.clone();
    where_.push(or_all(&[
        range_error.clone(),
        and_all(&[ant_tail, not_cons]),
    ]));
    let mut scope = Rendered {
        from: ant.from.clone(),
        laterals: ant.laterals.clone(),
        where_: ant.where_.clone(),
        range_errors: Vec::new(),
        ..Rendered::default()
    };
    scope.laterals.extend(cons.laterals.iter().cloned());
    if range_error != "false" {
        scope.range_errors.push(range_error.clone());
    }
    Ok((
        format!(
            "SELECT {select},\n       {range_error} AS \"range_error\"\nFROM {}\nWHERE {}",
            from_list(&scope),
            where_.join("\n  AND ")
        ),
        format!("\nORDER BY {order}\nLIMIT 1"),
        range_query(&scope),
        ant.env,
    ))
}

/// Whether any row of the scope has a range error, over the whole
/// scope and in no order. `None` when nothing in scope can have one.
fn range_query(scope: &Rendered) -> Option<String> {
    if !scope.has_sum() {
        return None;
    }
    Some(format!(
        "SELECT 1\nFROM {}\nWHERE {}\nLIMIT 1",
        from_list(scope),
        and_all(&[scope.prefix(), scope.range_error()])
    ))
}

/// Any other top-level shape: the invariant holds iff the body matches at
/// all, so violation is bare non-existence, with an empty witness.
fn generic_denial(body: &Prop, ctx: &mut Ctx<'_>) -> Result<Denial, CompileReason> {
    let r = render_prop(body, Env::new(), ctx)?;
    if !r.has_sum() {
        return Ok((
            format!(
                "SELECT 1 AS \"w\", false AS \"range_error\"\nWHERE NOT {}",
                r.exists_sql()
            ),
            String::new(),
            None,
            Env::new(),
        ));
    }
    // The kernel evaluates the sum for every prefix match, so a range
    // error anywhere dominates; otherwise the body must match somewhere.
    let scope = |condition: String| {
        format!(
            "EXISTS (SELECT 1 FROM {} WHERE {})",
            from_list(&r),
            and_all(&[r.prefix(), condition])
        )
    };
    let range = scope(r.range_error());
    let holds = scope(r.tail.clone().unwrap_or_else(|| "true".to_string()));
    Ok((
        format!("SELECT 1 AS \"w\", {range} AS \"range_error\"\nWHERE {range} OR NOT {holds}"),
        String::new(),
        None,
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
    if l.has_sum() || r.has_sum() {
        return Err(CompileReason::SumShape {
            detail: "sum in a filter-only implication",
        });
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
        format!(
            "SELECT 1 AS \"w\", false AS \"range_error\"\nWHERE {}",
            violated.exists_sql()
        ),
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
    // Order by the extractor expressions, not the generators' raw
    // `arguments`: raw-argument order matches the PK, which baits the
    // planner into an early-stop scan of the whole predicate (measured
    // plan flip at N=100k in the spike); the extractor expressions match
    // the partial expression indexes rung 2 derives. Deterministic in
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

/// How a claim-argument position is read in SQL so that SQL equality is
/// kernel equality - and, from the same object, how an index over that
/// position is expressed, so the index necessarily matches the
/// extractor. See the module doc for the per-kind proofs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Representation {
    Text,
    Numeric,
    /// The whole tagged value; sound only for kinds whose canonical
    /// serialisation makes structural equality semantic equality.
    Jsonb,
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
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Representation::Text => "text",
            Representation::Numeric => "numeric",
            Representation::Jsonb => "jsonb",
        }
    }
}

fn repr_for(kind: &PredicateArgKind) -> Result<Representation, CompileReason> {
    match kind {
        PredicateArgKind::Decimal => Ok(Representation::Numeric),
        PredicateArgKind::Subject => Ok(Representation::Text),
        PredicateArgKind::Bool
        | PredicateArgKind::Date
        | PredicateArgKind::Timestamp
        | PredicateArgKind::Duration => Ok(Representation::Jsonb),
        PredicateArgKind::Quantity(_)
        | PredicateArgKind::Collection
        | PredicateArgKind::Any
        | PredicateArgKind::CalendarSpan => Err(CompileReason::ArgumentKind { kind: kind.clone() }),
    }
}

fn col_sql(col: &ColRef, repr: Representation) -> String {
    repr.extractor(&format!("{}.", col.alias), col.position)
}

fn col_eq(a: &ColRef, b: &ColRef) -> Result<String, CompileReason> {
    let repr = repr_for(&a.kind)?;
    Ok(format!("({}) = ({})", col_sql(a, repr), col_sql(b, repr)))
}

/// A literal filter on a claim position, or a refusal when the literal
/// kind is outside the fragment.
fn literal_sql(value: &Value) -> Result<(String, Representation), CompileReason> {
    match value {
        Value::Subject(s) => Ok((quote_literal(s.as_str()), Representation::Text)),
        Value::Decimal(d) => Ok((
            format!("{}::numeric", quote_literal(d)),
            Representation::Numeric,
        )),
        Value::Date(_) => Err(CompileReason::Literal { kind: "date" }),
        Value::Timestamp(_) => Err(CompileReason::Literal { kind: "timestamp" }),
        Value::Duration(_) => Err(CompileReason::Literal { kind: "duration" }),
        Value::CalendarSpan(_) => Err(CompileReason::Literal {
            kind: "calendar span",
        }),
        Value::Quantity { .. } => Err(CompileReason::Literal { kind: "quantity" }),
    }
}

/// Stage-2 constant equality on an antecedent column, or None when the
/// value kind cannot be rendered (widens to Unbounded). The tagged
/// tier compares the whole value as jsonb against the delta value's
/// own serialisation - the same serde every stored claim passed
/// through, so the constant and the column speak one canonical form.
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
        _ => None,
    }
}

fn literal_matches(lit: &Value, ev: &EvalValue) -> bool {
    match (lit, ev) {
        (Value::Subject(a), EvalValue::Subject(b)) => a == b,
        (Value::Decimal(a), EvalValue::Decimal(b)) => a.parse::<Decimal>().is_ok_and(|a| a == *b),
        // Guard kinds the fragment cannot compare: treat as matching, which
        // only widens the touched-case set.
        _ => true,
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
                    // bindings the sum comparison admitted; the query
                    // computes every row's sum. Only a closing sum
                    // comparison has one evaluation boundary.
                    return Err(CompileReason::SumShape {
                        detail: "a sum comparison must be the last conjunct of its scope",
                    });
                }
                let r = render_prop(p, acc.env.clone(), ctx)?;
                acc.from.extend(r.from);
                acc.laterals.extend(r.laterals);
                acc.where_.extend(r.where_);
                acc.range_errors.extend(r.range_errors);
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
        Prop::Eq(a, b) => compare_sql(a, b, "=", &env, ctx),
        Prop::Neq(a, b) => compare_sql(a, b, "<>", &env, ctx),
        Prop::Compare {
            op,
            domain,
            left,
            right,
        } => {
            if *domain != OrderedDomain::Decimal {
                return Err(CompileReason::ComparisonDomain { domain: *domain });
            }
            let op = match op {
                morpholog_core::CompareOp::Le => "<=",
                morpholog_core::CompareOp::Lt => "<",
                morpholog_core::CompareOp::Ge => ">=",
                morpholog_core::CompareOp::Gt => ">",
            };
            compare_sql(left, right, op, &env, ctx)
        }
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
    let mut guards = Vec::new();
    let mut var_map = Vec::new();
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
                let (lit, repr) = literal_sql(v)?;
                where_.push(format!("({}) = {}", col_sql(&col, repr), lit));
                ctx.required.insert((predicate.clone(), i, repr));
                guards.push((i, v.clone()));
            }
            Term::Var(v) => {
                var_map.push((i, v.clone()));
                if let Some(bound) = env.get(v) {
                    where_.push(col_eq(bound, &col)?);
                    let repr = repr_for(&col.kind)?;
                    ctx.required
                        .insert((bound.predicate.clone(), bound.position, repr));
                    ctx.required.insert((predicate.clone(), i, repr));
                } else {
                    // Binding a variable requires the position to carry a
                    // proved equality representation NOW, not lazily: a
                    // later join or witness read must never fall back to
                    // an unsound comparison.
                    repr_for(&col.kind)?;
                    env.insert(v.clone(), col);
                }
            }
        }
    }
    ctx.occurrences.push((predicate.clone(), guards, var_map));
    Ok(Rendered {
        from: vec![(alias.clone(), format!("morpholog.claims {alias}"))],
        where_,
        env,
        ..Rendered::default()
    })
}

/// A scope the query evaluates by existence (a nested negation,
/// exists or implication) may stop at its first match, where the
/// kernel evaluates every binding of a sum's scope; no sum compiles
/// under one.
fn nested_scope(r: Rendered) -> Result<Rendered, CompileReason> {
    if r.has_sum() {
        return Err(CompileReason::SumShape {
            detail: "sum under a nested scope",
        });
    }
    Ok(r)
}

fn compare_sql(
    a: &ValueExpr,
    b: &ValueExpr,
    op: &str,
    env: &Env,
    ctx: &mut Ctx<'_>,
) -> Result<Rendered, CompileReason> {
    let a_sql = value_sql(a, env, ctx)?;
    let b_sql = value_sql(b, env, ctx)?;
    let clause = format!("({a_sql}) {op} ({b_sql})");
    let sums = std::mem::take(&mut ctx.pending_sums);
    if sums.is_empty() {
        return Ok(Rendered {
            where_: vec![clause],
            env: env.clone(),
            ..Rendered::default()
        });
    }
    Ok(Rendered {
        laterals: sums.iter().map(|s| s.lateral.clone()).collect(),
        tail: Some(clause),
        range_errors: sums.into_iter().map(|s| s.range_error).collect(),
        env: env.clone(),
        ..Rendered::default()
    })
}

fn value_sql(expr: &ValueExpr, env: &Env, ctx: &mut Ctx<'_>) -> Result<String, CompileReason> {
    match expr {
        ValueExpr::Term(Term::Var(v)) => {
            let col = env.get(v).ok_or_else(|| CompileReason::UnvalidatedShape {
                detail: format!("unbound variable in value position: {v}"),
            })?;
            Ok(col_sql(col, repr_for(&col.kind)?))
        }
        ValueExpr::Term(Term::Literal(v)) => {
            let (lit, _) = literal_sql(v)?;
            Ok(lit)
        }
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
            if r.has_sum() {
                return Err(CompileReason::SumShape {
                    detail: "sum within a sum body",
                });
            }
            // The compiled sum target is a bound decimal variable or a
            // decimal literal - the pre-expression-target shape. A
            // computed target refuses.
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
            Ok(total)
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

#[cfg(test)]
mod tests;
