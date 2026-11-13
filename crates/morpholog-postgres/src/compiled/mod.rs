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
//! since the epoch, the kernel's own precision), a date as
//! `morpholog.date_ordinal` (year, month and day as one integer). Where the kernel can
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
//! A defined call joins its body into the caller's chain under the call's
//! frame: a parameter bound to a caller column becomes a correlated
//! equality, one bound to a literal a filter, a free one binds from the
//! body, and only parameters come back to the caller. The error plan
//! renders the body a second time and asks it whole per caller row, as
//! the kernel does. A call under a sum is refused: a sum sees how many
//! witnesses a call has, and the kernel counts each projection once.
//!
//! Witness contract: rule name, version and witness match the kernel's,
//! values included. Once a rejection is established, the diagnosis
//! follows the kernel's first failing path, in its order, into the call
//! that killed the chain, whose frame replaces the caller's variables:
//! which row fails, and where, decides which variables the witness
//! carries, not only their values. The full check answers for the whole
//! state, so on dirty history it can name another case, with the same
//! variables.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;

use morpholog_core::{Admission, EffectiveDelta, Outcome, StagedDelta, State, execution};
use morpholog_core::{
    ClaimInstance, CompareOp, Compared, Definition, DefinitionName, EvalError, EvalValue, Impact,
    ImpactPlan, Invariant, InvariantName, OrderedDomain, PredicateArgKind, PredicateDecl,
    PredicateName, Prop, RejectionReason, SumSeed, Term, ValidatedProgram, Value, ValueExpr, Var,
    WitnessBinding, ordered_compare_error,
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
    /// A construct the fragment has no rendering for (`or`, `pre`,
    /// arithmetic, ...).
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
    /// An `or` outside the compiled shape: it compiles only as the last
    /// conjunct of a consequent, exporting no binding, so a conjunct
    /// after it or a call that would bind a parameter through it is
    /// refused.
    DisjunctionShape { detail: &'static str },
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
            CompileReason::DisjunctionShape { detail } => {
                write!(f, "`or` outside the compiled shape: {detail}")
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
    /// Every index the set's SQL can seek on, with the invariant whose
    /// SQL seeks on it, once per pair.
    pub(crate) fn index_needs(&self) -> impl Iterator<Item = (&IndexSpec, &InvariantName)> {
        self.invariants.iter().flat_map(|inv| {
            inv.required_indexes
                .iter()
                .chain(&inv.case_indexes)
                .map(move |spec| (spec, &inv.name))
        })
    }

    /// Every index the set's SQL can seek on, once each, in
    /// specification order.
    #[cfg(test)]
    pub(crate) fn required_indexes(&self) -> Vec<IndexSpec> {
        let specs: std::collections::BTreeSet<IndexSpec> =
            self.index_needs().map(|(spec, _)| spec.clone()).collect();
        specs.into_iter().collect()
    }
}

/// Which check runs: the whole-state check, or the check bounded to the
/// cases the effective delta could have changed, which production runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// Used by the differential: on governed history it must agree with
    /// `CaseBound`; on dirty history it is the whole-state question
    /// `evaluate` asks.
    #[cfg_attr(not(test), expect(dead_code, reason = "only the differentials use it"))]
    Full,
    CaseBound,
}

/// A violation the runner found: the rule, its version, its witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SqlViolation {
    pub(crate) name: InvariantName,
    pub(crate) version: u32,
    pub(crate) witness: Vec<WitnessBinding>,
    /// The comparison the diagnosis blamed, with its operands off the
    /// chosen row, as the kernel's walk would report it.
    pub(crate) compared: Option<Compared>,
}

impl From<SqlViolation> for RejectionReason {
    fn from(v: SqlViolation) -> Self {
        RejectionReason::Invariant {
            name: v.name,
            version: v.version,
            witness: v.witness,
            compared: v.compared,
        }
    }
}

/// What the kernel needs to decide one invariant itself, where a compiled
/// sum meets a value other than the decimals it adds, as history written
/// under an older programme can hold: the programme's admission, and the
/// delta the claims table reported for this act.
pub(crate) struct KernelFallback<'a> {
    pub(crate) admission: &'a Admission<'a>,
    pub(crate) effective: &'a EffectiveDelta,
}

impl KernelFallback<'_> {
    /// The kernel's verdict on the invariant `name`, case-local over this
    /// act's effective delta, from the state before it.
    async fn verdict(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        name: &InvariantName,
        steps: &[DeltaStep],
    ) -> Result<Option<SqlViolation>, PgError> {
        let invariants = self.admission.invariants();
        let index = invariants
            .iter()
            .position(|i| &i.name == name)
            .ok_or_else(|| PgError::InvalidState(format!("no invariant {name} to decide")))?;
        let mut predicates = BTreeSet::new();
        morpholog_core::predicates_referenced_by_prop(
            &invariants[index].body,
            self.admission.definitions(),
            &mut predicates,
        );
        let pre = self.pre_state(tx, &predicates, steps).await?;
        let staged = StagedDelta::Staged {
            asserted: self.effective.asserted.clone(),
            retracted: self.effective.retracted.clone(),
            emitted: Vec::new(),
        };
        let outcome = execution::finish_staged_delta_with_effective(
            staged,
            &pre,
            &execution::admission_range(self.admission, index..index + 1),
            self.effective,
        )
        .map_err(PgError::Kernel)?;
        match outcome {
            Outcome::Accepted { .. } => Ok(None),
            Outcome::Rejected {
                reason:
                    RejectionReason::Invariant {
                        name,
                        version,
                        witness,
                        compared,
                    },
            } => Ok(Some(SqlViolation {
                name,
                version,
                witness,
                compared,
            })),
            Outcome::Rejected { reason } => Err(PgError::InvalidState(format!(
                "an invariant check refused by another rule: {reason}"
            ))),
        }
    }

    /// The claims of `predicates` as the kernel held them before this act:
    /// the table, which already holds the act's delta, with the act's
    /// admissions taken out and its retractions put back. In the kernel's
    /// order: what this transaction's acts did not write, by key, then each
    /// earlier act's admissions as it made them.
    async fn pre_state(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        predicates: &BTreeSet<PredicateName>,
        steps: &[DeltaStep],
    ) -> Result<State, PgError> {
        let names: Vec<String> = predicates.iter().map(ToString::to_string).collect();
        let rows = sqlx::query!(
            r#"SELECT predicate_name, arguments, arguments_hash, asserted_in
               FROM morpholog.claims
               WHERE predicate_name = ANY($1::text[])"#,
            &names
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(classify)?;
        let earlier = steps.split_last().map_or(&[][..], |(_, earlier)| earlier);
        let made_by = |claim: &ClaimInstance| {
            earlier.iter().enumerate().find_map(|(step, s)| {
                s.asserted
                    .iter()
                    .position(|c| c == claim)
                    .map(|place| (step, place))
            })
        };
        let mut base: Vec<(String, Vec<u8>, ClaimInstance)> = Vec::new();
        let mut overlay: Vec<((usize, usize), ClaimInstance)> = Vec::new();
        for row in rows {
            let claim = ClaimInstance {
                predicate: PredicateName::from(row.predicate_name.as_str()),
                args: serde_json::from_value(row.arguments)?,
            };
            if self.effective.asserted.contains(&claim) {
                continue;
            }
            let written_here = earlier.iter().any(|s| s.transition_id == row.asserted_in);
            match made_by(&claim).filter(|_| written_here) {
                Some(place) => overlay.push((place, claim)),
                None => base.push((row.predicate_name, row.arguments_hash, claim)),
            }
        }
        for claim in &self.effective.retracted {
            if !predicates.contains(&claim.predicate) {
                continue;
            }
            match made_by(claim) {
                Some(place) => overlay.push((place, claim.clone())),
                None => {
                    let hash = crate::claim_key::arguments_hash(&claim.args)
                        .map_err(PgError::InvalidState)?;
                    base.push((claim.predicate.to_string(), hash.to_vec(), claim.clone()));
                }
            }
        }
        base.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        overlay.sort_by_key(|(place, _)| *place);
        let mut state = State::from_claims(base.into_iter().map(|(_, _, c)| c).collect());
        let admitted: Vec<ClaimInstance> = overlay.into_iter().map(|(_, c)| c).collect();
        state.apply(&admitted, &[]);
        Ok(state)
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
    /// violation, as it does in the kernel. With no error, the diagnostic
    /// query finds the witness in the same order. Where the first erroring
    /// binding is a sum meeting a value other than a decimal, the kernel
    /// decides that invariant instead (`fallback`).
    /// Production walks runs; the differentials check a set whole.
    #[cfg_attr(not(test), expect(dead_code, reason = "only the differentials use it"))]
    pub(crate) async fn first_violation(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        stage: Stage,
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
        steps: &[DeltaStep],
        fallback: &KernelFallback<'_>,
    ) -> Result<Option<SqlViolation>, PgError> {
        self.first_violation_in(
            0..self.invariants.len(),
            tx,
            stage,
            asserted,
            retracted,
            steps,
            fallback,
        )
        .await
    }

    /// [`Self::first_violation`] over one run of the set's invariants, so
    /// an execution that interleaves this evaluator with the kernel keeps
    /// the programme's order.
    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is a distinct input the caller already holds"
    )]
    pub(crate) async fn first_violation_in(
        &self,
        run: std::ops::Range<usize>,
        tx: &mut Transaction<'_, Postgres>,
        stage: Stage,
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
        steps: &[DeltaStep],
        fallback: &KernelFallback<'_>,
    ) -> Result<Option<SqlViolation>, PgError> {
        'invariants: for inv in &self.invariants[run] {
            let cases = match stage {
                Stage::Full => None,
                Stage::CaseBound => match inv.case_filter(asserted, retracted) {
                    CaseFilter::Untouched => continue,
                    CaseFilter::Bounded(cases) => Some(cases),
                    CaseFilter::Unbounded => None,
                },
            };
            let case_filter = cases.as_ref().map(BoundedCases::filter);
            let sql = inv.violation_sql(case_filter.as_deref());
            // Safe: this module renders the SQL from a validated programme,
            // with identifiers quoted, literals escaped and the name comment
            // neutralised.
            let row = sqlx::query(sqlx::AssertSqlSafe(sql))
                .fetch_optional(&mut **tx)
                .await
                .map_err(classify)?;
            if row.is_none() {
                continue;
            }
            for error_sql in inv.error_sqls(case_filter.as_deref(), steps)? {
                let erroring = sqlx::query(sqlx::AssertSqlSafe(error_sql))
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(classify)?;
                if let Some(erroring) = erroring {
                    match decode_error(&erroring, &inv.values)? {
                        Some(error) => return Err(PgError::Kernel(error)),
                        None => match fallback.verdict(tx, &inv.name, steps).await? {
                            Some(violation) => return Ok(Some(violation)),
                            None => continue 'invariants,
                        },
                    }
                }
            }
            let (witness, compared) = inv.diagnose(tx, cases.as_ref(), steps).await?;
            return Ok(Some(SqlViolation {
                name: inv.name.clone(),
                version: inv.version,
                witness,
                compared,
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

/// The blamed comparison's two values. A column reads off its chosen
/// row and a literal is itself; a sum is computed once more, under the
/// chosen rows and the prefix that still matched, the comparison kept
/// out of the selection so the row is there to read. Nothing is reported
/// when a side cannot be read, or when the values do not fail the
/// comparison: a diagnosis that disagrees with the decision says nothing
/// rather than something wrong.
async fn compared_off(
    tx: &mut Transaction<'_, Postgres>,
    label: &str,
    chosen: &Chosen,
    prefix: &str,
    leaf: &LeafCompare,
) -> Result<Option<Compared>, PgError> {
    let mut values = Vec::with_capacity(2);
    for side in [&leaf.left, &leaf.right] {
        let value = match side {
            Side::Bound(bound) => chosen.value(bound).ok(),
            Side::Sum { lateral, total } => {
                let sql = format!(
                    "{label}\nSELECT ({total})::text AS \"t\"{}\nWHERE {}\nLIMIT 1",
                    chosen.sources_clause(lateral),
                    chosen.pins(prefix)
                );
                match fetch_diagnostic(tx, sql).await? {
                    Some(row) => row
                        .try_get::<String, _>("t")
                        .ok()
                        .and_then(|t| t.parse::<rust_decimal::Decimal>().ok())
                        .map(EvalValue::Decimal),
                    None => None,
                }
            }
            Side::Computed { expr, vars, sums } => {
                computed_off(tx, label, chosen, prefix, expr, vars, sums).await?
            }
        };
        let Some(value) = value else {
            return Ok(None);
        };
        values.push(value);
    }
    let mut values = values.into_iter();
    let (Some(left), Some(right)) = (values.next(), values.next()) else {
        return Ok(None);
    };
    let compared = Compared {
        op: leaf.op.to_string(),
        left,
        right,
    };
    Ok((compared.holds() == Some(false)).then_some(compared))
}

/// A computed operand's value under the chosen rows, as the kernel
/// evaluates it; `None` when any part of it errs there.
async fn computed_off(
    tx: &mut Transaction<'_, Postgres>,
    label: &str,
    chosen: &Chosen,
    prefix: &str,
    expr: &ValueExpr,
    vars: &[(Var, Bound)],
    sums: &[SumPart],
) -> Result<Option<EvalValue>, PgError> {
    let mut bindings = std::collections::HashMap::new();
    for (var, bound) in vars {
        let Ok(value) = chosen.value(bound) else {
            return Ok(None);
        };
        bindings.insert(var.clone(), value);
    }
    let mut results = Vec::with_capacity(sums.len());
    for sum in sums {
        let sql = format!(
            "{label}\nSELECT trim_scale({})::text AS \"t\", (({}) OR ({})) AS \"e\"{}\nWHERE {}\nLIMIT 1",
            sum.total,
            sum.kind,
            sum.range,
            chosen.sources_clause(&sum.lateral),
            chosen.pins(prefix)
        );
        let Some(row) = fetch_diagnostic(tx, sql).await? else {
            return Ok(None);
        };
        if row.try_get::<bool, _>("e").unwrap_or(true) {
            return Ok(None);
        }
        let Some(total) = row
            .try_get::<String, _>("t")
            .ok()
            .and_then(|t| t.parse::<rust_decimal::Decimal>().ok())
        else {
            return Ok(None);
        };
        results.push(Ok(EvalValue::Decimal(total)));
    }
    Ok(morpholog_core::eval_value_over(expr, &bindings, &results).and_then(Result::ok))
}

/// A query run only to explain a refusal already decided. A statement
/// error in it must not replace the refusal, and would leave the
/// transaction aborted, so it runs under a savepoint: on an error the
/// savepoint is rolled back and the answer is simply none. An error on
/// the savepoint itself is the connection's, and operational.
pub async fn fetch_diagnostic(
    tx: &mut Transaction<'_, Postgres>,
    sql: String,
) -> Result<Option<sqlx::postgres::PgRow>, PgError> {
    sqlx::raw_sql("SAVEPOINT morpholog_diagnosis")
        .execute(&mut **tx)
        .await
        .map_err(classify)?;
    let fetched = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_optional(&mut **tx)
        .await;
    match fetched {
        Ok(row) => {
            sqlx::raw_sql("RELEASE SAVEPOINT morpholog_diagnosis")
                .execute(&mut **tx)
                .await
                .map_err(classify)?;
            Ok(row)
        }
        Err(_) => {
            sqlx::raw_sql("ROLLBACK TO SAVEPOINT morpholog_diagnosis")
                .execute(&mut **tx)
                .await
                .map_err(classify)?;
            Ok(None)
        }
    }
}

async fn fetch(
    tx: &mut Transaction<'_, Postgres>,
    sql: String,
) -> Result<Option<sqlx::postgres::PgRow>, PgError> {
    // Safe: this module renders the SQL from a validated programme, with
    // identifiers quoted, literals escaped and the name comment
    // neutralised.
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_optional(&mut **tx)
        .await
        .map_err(classify)
}

/// The kernel's error for the first erroring binding: a sum's range, or
/// the kernel's own verdict on the two operands it could not order. None
/// when a sum met a value other than a decimal: the kernel may add it,
/// refuse to, or add it and fail later, so the kernel decides.
fn decode_error(
    row: &sqlx::postgres::PgRow,
    values: &[ValueExpr],
) -> Result<Option<EvalError>, PgError> {
    let column = |name: &str| -> Result<String, PgError> {
        row.try_get::<Option<String>, _>(name)
            .map_err(|e| PgError::InvalidState(format!("error column {name} missing: {e}")))?
            .ok_or_else(|| PgError::InvalidState(format!("error column {name} is null")))
    };
    match column("kind")?.as_str() {
        "range" => Ok(Some(EvalError::sum_out_of_decimal_range())),
        "sum_kind" => Ok(None),
        "compare" => {
            let domain = match column("domain")?.as_str() {
                "decimal" => OrderedDomain::Decimal,
                "timestamp" => OrderedDomain::Timestamp,
                "date" => OrderedDomain::Date,
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
            ordered_compare_error(domain, &left, &right)
                .map(Some)
                .ok_or_else(|| {
                    PgError::InvalidState(format!(
                        "the compiled check reported an error the kernel does not raise for {left:?} vs {right:?}"
                    ))
                })
        }
        "value" => {
            let index: usize = column("domain")?.parse().map_err(|e| {
                PgError::InvalidState(format!("error value index undecodable: {e}"))
            })?;
            let expr = values.get(index).ok_or_else(|| {
                PgError::InvalidState(format!("error value index {index} names no computed value"))
            })?;
            let payload: serde_json::Value =
                serde_json::from_str(&column("left")?).map_err(|e| {
                    PgError::InvalidState(format!("error value payload undecodable: {e}"))
                })?;
            let decoded = arith::decode(&payload).ok_or_else(|| {
                PgError::InvalidState(format!("error value payload malformed: {payload}"))
            })?;
            let arith::Decoded::Values(bindings, sums) = decoded else {
                return Ok(None);
            };
            match morpholog_core::eval_value_over(expr, &bindings, &sums) {
                Some(Err(e)) => Ok(Some(e)),
                other => Err(PgError::InvalidState(format!(
                    "the compiled check reported an error the kernel does not raise for {payload}: {other:?}"
                ))),
            }
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
    #[cfg_attr(not(test), expect(dead_code, reason = "only the differentials use it"))]
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

/// Statistics on one position's seek expression, across the whole claims
/// table. The indexes are partial, and the planner does not estimate a
/// filter from a partial index's statistics: without these it guesses, and
/// a selective seek looks like a walk of the predicate. One object serves
/// every predicate's index at the position.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct StatisticsSpec {
    pub(crate) position: usize,
}

impl StatisticsSpec {
    /// Named for the seek representation and the position, so statistics
    /// on another key are another object.
    pub(crate) fn name(&self) -> String {
        format!("morpholog_cs_vk1_p{}", self.position)
    }

    /// The specification an object under this exact name is expected to
    /// be, or none for any other name: another representation, a
    /// position spelled otherwise than this crate spells it, or a stray
    /// suffix is not this build's object to reconcile.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        let digits = name.strip_prefix("morpholog_cs_vk1_p")?;
        let position: usize = digits.parse().ok()?;
        (position.to_string() == digits).then_some(Self { position })
    }

    pub(crate) fn expression_sql(&self) -> String {
        seek_expression("", self.position)
    }

    pub(crate) fn create_sql(&self) -> String {
        format!(
            "CREATE STATISTICS morpholog.{} ON ({}) FROM morpholog.claims",
            quote_ident(&self.name()),
            self.expression_sql()
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
    /// The touched cases, each a SQL conjunction over the antecedent's
    /// columns, in the plan's order.
    Bounded(BoundedCases),
    /// Touched, but not boundable to antecedent columns: run the full check.
    Unbounded,
}

/// The touched cases rendered onto the antecedent's columns, in the
/// order the impact plan lists them: the filter is their disjunction,
/// and the rank says which case a row belongs to first, so the
/// diagnosis picks the earliest case's first failing row, as the kernel
/// does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundedCases {
    disjuncts: Vec<String>,
}

impl BoundedCases {
    /// The disjunction, parenthesised; `(false)` for no case at all.
    pub(crate) fn filter(&self) -> String {
        if self.disjuncts.is_empty() {
            return "(false)".to_string();
        }
        format!("({})", self.disjuncts.join(") OR ("))
    }

    /// The index of the first case a row matches, for an `ORDER BY`.
    fn rank(&self) -> String {
        let mut sql = String::from("CASE");
        for (i, d) in self.disjuncts.iter().enumerate() {
            let _ = write!(sql, " WHEN ({d}) THEN {i}");
        }
        sql.push_str(" END");
        sql
    }
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
    /// Which cases a delta touches, decided by core; `case_cols` renders
    /// its bindings onto the antecedent's columns.
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
    /// Where the witness comes from; `None` when the kernel reports none.
    diagnosis: Option<Level>,
    /// The computed values a `value` error report names by index.
    values: Vec<ValueExpr>,
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

/// How the kernel explains a rejection, before its runtime parts. It runs
/// only once a rejection is established. The kernel takes the first
/// violating antecedent match in its order, then diagnoses the consequent
/// conjunct by conjunct: under the first row of the longest prefix that
/// still matched, it blames the conjunct that killed the chain, and
/// descends into it when that is a defined call (whose frame replaces the
/// caller's variables), a nested conjunction, or a nested implication
/// (whose first failing antecedent match it takes next). The violation
/// query cannot stand in for it: another violating row can fail at
/// another depth, or inside another call, and so carry other variables.
#[derive(Debug)]
struct Level {
    /// The rows this level starts from: the first match, in the kernel's
    /// order, of an antecedent whose consequent fails.
    choice: Option<Choice>,
    /// The variables in scope on entry.
    entry: Vec<(Var, Bound)>,
    /// Each conjunct's cumulative prefix, in order.
    prefixes: Vec<Prefix>,
    /// Where the kernel goes when the chain dies at a conjunct.
    descents: BTreeMap<usize, Level>,
    /// The conjuncts that are a comparison or an equality, with their
    /// operands as the kernel would read them under the bindings at that
    /// point: the two values a refusal reports when the chain dies there.
    leaves: BTreeMap<usize, LeafCompare>,
}

/// A comparison the diagnosis can blame, with its operands.
#[derive(Debug)]
struct LeafCompare {
    op: &'static str,
    left: Side,
    right: Side,
}

/// One operand of a blamed comparison: a column of a chosen row, a
/// literal, or a sum computed once more under the chosen rows.
#[derive(Debug)]
enum Side {
    Bound(Bound),
    Sum {
        lateral: String,
        total: String,
    },
    /// Arithmetic or a builtin over bindings and sums: the kernel
    /// evaluates it over the chosen rows' values.
    Computed {
        expr: ValueExpr,
        vars: Vec<(Var, Bound)>,
        sums: Vec<SumPart>,
    },
}

/// The variables a computed value reads outside its sums.
fn computed_vars(expr: &ValueExpr, env: &Env, out: &mut Vec<(Var, Bound)>) {
    match expr {
        ValueExpr::Term(Term::Var(v)) => {
            if !out.iter().any(|(seen, _)| seen == v)
                && let Some(bound) = env.get(v)
            {
                out.push((v.clone(), bound.clone()));
            }
        }
        ValueExpr::Arith { left, right, .. } => {
            computed_vars(left, env, out);
            computed_vars(right, env, out);
        }
        ValueExpr::Call { args, .. } => {
            for arg in args {
                computed_vars(arg, env, out);
            }
        }
        _ => {}
    }
}

/// The operand of a comparison the diagnosis may blame, or `None` for a
/// form the compiler does not render, which the comparison itself would
/// have refused.
fn leaf_side(expr: &ValueExpr, env: &Env, ctx: &mut Ctx<'_>) -> Option<Side> {
    match expr {
        ValueExpr::Term(Term::Var(v)) => env.get(v).cloned().map(Side::Bound),
        ValueExpr::Term(Term::Literal(value)) => Some(Side::Bound(Bound::Lit(value.clone()))),
        ValueExpr::Sum { .. } => {
            let operand = value_sql(expr, env, ctx).ok()?;
            let Some(Pending::Sum(rendered)) = ctx.pending.pop() else {
                return None;
            };
            let Ordered::Numeric { sql, .. } = operand.ordered else {
                return None;
            };
            Some(Side::Sum {
                lateral: rendered.lateral,
                total: sql,
            })
        }
        ValueExpr::Arith { .. } | ValueExpr::Call { .. } => {
            let start = ctx.pending.len();
            let computed = value_sql(expr, env, ctx);
            let rendered: Vec<Pending> = ctx.pending.drain(start..).collect();
            computed.ok()?;
            let sums = rendered.into_iter().find_map(|p| match p {
                Pending::Value(Probe {
                    report: ErrorReport::Value { sums, .. },
                    ..
                }) => Some(sums),
                _ => None,
            })?;
            let mut vars = Vec::new();
            computed_vars(expr, env, &mut vars);
            Some(Side::Computed {
                expr: expr.clone(),
                vars,
                sums,
            })
        }
        _ => None,
    }
}

/// The comparison a conjunct is, if it is one, with its operands under
/// `env`.
fn leaf_compare(conjunct: &Prop, env: &Env, ctx: &mut Ctx<'_>) -> Option<LeafCompare> {
    let (op, left, right) = match conjunct {
        Prop::Compare {
            op,
            domain,
            left,
            right,
        } => (
            morpholog_core::format::compare_token(*op, *domain),
            left,
            right,
        ),
        Prop::Eq(left, right) => ("=", left, right),
        Prop::Neq(left, right) => ("!=", left, right),
        _ => return None,
    };
    Some(LeafCompare {
        op,
        left: leaf_side(left, env, ctx)?,
        right: leaf_side(right, env, ctx)?,
    })
}

#[derive(Debug)]
struct Choice {
    from: String,
    where_: String,
    aliases: Vec<(String, PredicateName)>,
}

#[derive(Debug)]
struct Prefix {
    from: String,
    where_: String,
    aliases: Vec<(String, PredicateName)>,
    /// The variables bound by the end of this conjunct beyond the entry.
    bound: Vec<(Var, ColRef)>,
}

fn conjuncts_of(prop: &Prop) -> Vec<&Prop> {
    match prop {
        Prop::And(ps) => ps.iter().collect(),
        other => vec![other],
    }
}

fn diagnostic_level(
    conjuncts: &[&Prop],
    env: Env,
    entry: Vec<(Var, Bound)>,
    choice: Option<Choice>,
    ctx: &mut Ctx<'_>,
) -> Result<Level, CompileReason> {
    let mut acc = Rendered {
        env,
        ..Rendered::default()
    };
    let mut prefixes = Vec::new();
    let mut descents = BTreeMap::new();
    let mut leaves = BTreeMap::new();
    for (k, conjunct) in conjuncts.iter().enumerate() {
        if let Some(leaf) = leaf_compare(conjunct, &acc.env, ctx) {
            leaves.insert(k, leaf);
        }
        match conjunct {
            Prop::Defined { name, args } => {
                let (def, frame) = call_frame(name, args, &acc.env, ctx)?;
                let body = condition_body(def)?;
                let entry = frame.iter().map(|(p, b)| (p.clone(), b.clone())).collect();
                descents.insert(
                    k,
                    diagnostic_level(&conjuncts_of(body), frame, entry, None, ctx)?,
                );
            }
            Prop::And(ps) => {
                let entry = acc
                    .env
                    .iter()
                    .map(|(v, b)| (v.clone(), b.clone()))
                    .collect();
                let inner: Vec<&Prop> = ps.iter().collect();
                descents.insert(
                    k,
                    diagnostic_level(&inner, acc.env.clone(), entry, None, ctx)?,
                );
            }
            Prop::Implies { left, right }
            | Prop::Forall {
                source: left,
                body: right,
                ..
            } => {
                let l = render_prop(left, acc.env.clone(), ctx)?;
                let r = render_prop(right, l.env.clone(), ctx)?;
                let mut where_ = l.where_.clone();
                where_.push(if r.from.is_empty() {
                    format!("NOT ({})", r.conjunction())
                } else {
                    format!("NOT {}", r.exists_sql())
                });
                let choice = Choice {
                    from: from_list(&l),
                    where_: joined(&where_),
                    aliases: l.aliases(),
                };
                let entry = l.env.iter().map(|(v, b)| (v.clone(), b.clone())).collect();
                descents.insert(
                    k,
                    diagnostic_level(&conjuncts_of(right), l.env, entry, Some(choice), ctx)?,
                );
            }
            _ => {}
        }
        let r = render_prop(conjunct, acc.env.clone(), ctx)?;
        acc.from.extend(r.from);
        acc.laterals.extend(r.laterals);
        acc.where_.extend(r.where_);
        acc.env = r.env;
        prefixes.push(Prefix {
            from: from_list(&acc),
            where_: acc.conjunction(),
            aliases: acc.aliases(),
            bound: columns(&acc.env)
                .into_iter()
                .filter(|(v, _)| !entry.iter().any(|(e, _)| e == v))
                .collect(),
        });
    }
    Ok(Level {
        choice,
        entry,
        prefixes,
        descents,
        leaves,
    })
}

/// The rows the diagnosis has chosen so far, by alias.
#[derive(Default)]
struct Chosen {
    rows: BTreeMap<String, (PredicateName, String, serde_json::Value)>,
}

impl Chosen {
    /// Each alias's primary key and arguments, to pin and read the row.
    fn select(aliases: &[(String, PredicateName)]) -> String {
        if aliases.is_empty() {
            return "1 AS \"w\"".to_string();
        }
        aliases
            .iter()
            .map(|(alias, _)| {
                format!(
                    "encode({alias}.arguments_hash, 'hex') AS {}, {alias}.arguments::text AS {}",
                    quote_ident(&format!("h_{alias}")),
                    quote_ident(&format!("a_{alias}"))
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn read(
        &mut self,
        aliases: &[(String, PredicateName)],
        row: &sqlx::postgres::PgRow,
    ) -> Result<(), PgError> {
        for (alias, predicate) in aliases {
            let hash: String = row
                .try_get(format!("h_{alias}").as_str())
                .map_err(|e| PgError::InvalidState(format!("diagnosis row: {e}")))?;
            let text: String = row
                .try_get(format!("a_{alias}").as_str())
                .map_err(|e| PgError::InvalidState(format!("diagnosis row: {e}")))?;
            let args = serde_json::from_str(&text)?;
            self.rows
                .insert(alias.clone(), (predicate.clone(), hash, args));
        }
        Ok(())
    }

    /// The chosen rows, pinned by primary key, ahead of `from`; no clause
    /// at all when there is nothing to read from.
    fn sources_clause(&self, from: &str) -> String {
        let sources: Vec<String> = self
            .rows
            .keys()
            .map(|alias| format!("morpholog.claims {alias}"))
            .chain((!from.is_empty()).then(|| from.to_string()))
            .collect();
        if sources.is_empty() {
            String::new()
        } else {
            format!("\nFROM {}", sources.join(", "))
        }
    }

    fn pins(&self, where_: &str) -> String {
        self.rows
            .iter()
            .map(|(alias, (predicate, hash, _))| {
                format!(
                    "{alias}.predicate_name = {} AND {alias}.arguments_hash = decode({}, 'hex')",
                    quote_literal(predicate.as_str()),
                    quote_literal(hash)
                )
            })
            .chain(std::iter::once(where_.to_string()))
            .collect::<Vec<_>>()
            .join("\n  AND ")
    }

    fn value(&self, bound: &Bound) -> Result<EvalValue, PgError> {
        match bound {
            Bound::Lit(value) => Ok(EvalValue::from(value)),
            Bound::Col(col) => {
                let (_, _, args) = self.rows.get(&col.alias).ok_or_else(|| {
                    PgError::InvalidState(format!("diagnosis read unchosen alias {}", col.alias))
                })?;
                Ok(serde_json::from_value(args[col.position].clone())?)
            }
        }
    }
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
                let mut sql = format!("SELECT {report}");
                // A scope with no sources of its own reads its probes on
                // the one empty binding: nothing to join, nothing to order.
                if !scope.from.is_empty() {
                    let _ = write!(sql, "\nFROM {}", scope.from);
                }
                let _ = write!(sql, "\nWHERE {}", scope.where_);
                if let Some(filter) = case_filter {
                    let _ = write!(sql, "\n  AND ({filter})");
                }
                if !scope.aliases.is_empty() {
                    let _ = write!(sql, "\nORDER BY {}", order(&scope.aliases)?);
                }
                sql.push_str("\nLIMIT 1");
                Ok(sql)
            })
            .collect()
    }

    /// The witness, found as the kernel finds it (see [`Level`]), and
    /// the comparison blamed where the chain died at one, its operands
    /// read off the chosen rows, a sum computed once more under them.
    async fn diagnose(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        cases: Option<&BoundedCases>,
        steps: &[DeltaStep],
    ) -> Result<(Vec<WitnessBinding>, Option<Compared>), PgError> {
        let Some(d) = &self.diagnosis else {
            return Ok((Vec::new(), None));
        };
        let order = |aliases: &[(String, PredicateName)]| -> Result<String, PgError> {
            Ok(aliases
                .iter()
                .map(|(alias, predicate)| order_keys(alias, predicate, steps))
                .collect::<Result<Vec<_>, _>>()?
                .join(", "))
        };
        let label = format!(
            "/* morpholog diagnosis {} v{} */",
            comment_safe(self.name.as_str()),
            self.version
        );
        let mut chosen = Chosen::default();
        let mut level = d;
        // The cases bound the invariant's own antecedent: the first level,
        // where the earliest case's rows come first.
        let mut bounded = cases;
        let mut witness = BTreeMap::new();
        loop {
            let level_cases = bounded.take();
            if let Some(choice) = &level.choice {
                let mut sql = format!(
                    "{label}\nSELECT {}{}\nWHERE {}",
                    Chosen::select(&choice.aliases),
                    chosen.sources_clause(&choice.from),
                    chosen.pins(&choice.where_)
                );
                if let Some(cases) = level_cases {
                    let _ = write!(sql, "\n  AND ({})", cases.filter());
                }
                let mut order_by = Vec::new();
                if let Some(cases) = level_cases {
                    order_by.push(cases.rank());
                }
                if !choice.aliases.is_empty() {
                    order_by.push(order(&choice.aliases)?);
                }
                if !order_by.is_empty() {
                    let _ = write!(sql, "\nORDER BY {}", order_by.join(", "));
                }
                sql.push_str("\nLIMIT 1");
                match fetch(tx, sql).await? {
                    Some(row) => chosen.read(&choice.aliases, &row)?,
                    // The kernel found nothing to descend into and blames
                    // the conjunct as a unit.
                    None if chosen.rows.is_empty() => {
                        return Err(PgError::InvalidState(format!(
                            "{} is violated but its diagnosis found no violation",
                            self.name
                        )));
                    }
                    None => break,
                }
            }
            witness = BTreeMap::new();
            for (var, bound) in &level.entry {
                witness.insert(var.clone(), chosen.value(bound)?);
            }
            let mut longest = None;
            for (s, prefix) in level.prefixes.iter().enumerate().rev() {
                let sql = format!(
                    "{label}\nSELECT {}{}\nWHERE {}\nORDER BY {}\nLIMIT 1",
                    Chosen::select(&prefix.aliases),
                    chosen.sources_clause(&prefix.from),
                    chosen.pins(&prefix.where_),
                    if prefix.aliases.is_empty() {
                        "1".to_string()
                    } else {
                        order(&prefix.aliases)?
                    }
                );
                if let Some(row) = fetch(tx, sql).await? {
                    chosen.read(&prefix.aliases, &row)?;
                    longest = Some(s);
                    break;
                }
            }
            // A conjunction that matched whole is blamed as a unit: the
            // kernel reports what was bound on entry.
            if let Some(s) = longest.filter(|s| s + 1 < level.prefixes.len()) {
                for (var, col) in &level.prefixes[s].bound {
                    witness.insert(var.clone(), chosen.value(&Bound::Col(col.clone()))?);
                }
            }
            let killed = longest.map_or(0, |s| s + 1);
            match level.descents.get(&killed) {
                Some(next) => level = next,
                None => {
                    // The chain died at a leaf. When it is a comparison,
                    // its operands are read under the successful prefix
                    // and the pinned rows, the comparison itself kept
                    // out of the selection, as the kernel's leaf reads
                    // them under the bindings it reached.
                    let compared = match level.leaves.get(&killed) {
                        Some(leaf) => {
                            let prefix = longest
                                .map(|s| level.prefixes[s].where_.as_str())
                                .unwrap_or("true");
                            compared_off(tx, &label, &chosen, prefix, leaf).await?
                        }
                        None => None,
                    };
                    return Ok((
                        witness
                            .into_iter()
                            .map(|(var, value)| WitnessBinding { var, value })
                            .collect(),
                        compared,
                    ));
                }
            }
        }
        Ok((
            witness
                .into_iter()
                .map(|(var, value)| WitnessBinding { var, value })
                .collect(),
            None,
        ))
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
        let mut disjuncts: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
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
            let disjunct = parts.join(" AND ");
            if seen.insert(disjunct.clone()) {
                disjuncts.push(disjunct);
            }
        }
        CaseFilter::Bounded(BoundedCases { disjuncts })
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
    let kind = case_over(probes, Column::Kind, order)?;
    let domain = case_over(probes, Column::Domain, order)?;
    let left = case_over(probes, Column::Left, order)?;
    let right = case_over(probes, Column::Right, order)?;
    Ok(format!(
        "{kind} AS \"kind\",\n       {domain} AS \"domain\",\n       {left} AS \"left\",\n       {right} AS \"right\""
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Column {
    Kind,
    Domain,
    Left,
    Right,
}

/// One report column across a scope's probes: the first probe that
/// fires on the row names it, as the kernel meets them.
fn case_over(
    probes: &[Probe],
    column: Column,
    order: &OrderRenderer<'_>,
) -> Result<String, PgError> {
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
        (ErrorReport::SumKind, Column::Kind) => Some("'sum_kind'".to_string()),
        (ErrorReport::Compare { .. }, Column::Kind) => Some("'compare'".to_string()),
        (ErrorReport::Value { .. }, Column::Kind) => Some("'value'".to_string()),
        (ErrorReport::Value { index, .. }, Column::Domain) => Some(format!("'{index}'")),
        (ErrorReport::Value { vars, sums, .. }, Column::Left) => {
            let vars: Vec<String> = vars
                .iter()
                .map(|(name, tagged)| format!("{}, {tagged}", quote_literal(name)))
                .collect();
            let mut parts = Vec::with_capacity(sums.len());
            for sum in sums {
                parts.push(format!(
                    "jsonb_build_object('total', trim_scale({})::text, 'kind', ({}), 'range', ({}))",
                    sum.total, sum.kind, sum.range
                ));
            }
            Some(format!(
                "(jsonb_build_object('vars', jsonb_build_object({}), 'sums', jsonb_build_array({})))::text",
                vars.join(", "),
                parts.join(", ")
            ))
        }
        (ErrorReport::Compare { domain, .. }, Column::Domain) => Some(match domain {
            OrderedDomain::Decimal => "'decimal'".to_string(),
            OrderedDomain::Timestamp => "'timestamp'".to_string(),
            OrderedDomain::Date => "'date'".to_string(),
            OrderedDomain::Duration => unreachable!("never rendered"),
        }),
        (ErrorReport::Compare { left, .. }, Column::Left) => Some(format!("({left})::text")),
        (ErrorReport::Compare { right, .. }, Column::Right) => Some(format!("({right})::text")),
        // A lifted scope's column is read on the first inner row where
        // any of its probes fires, from the first that does there.
        (
            ErrorReport::Lifted {
                from,
                where_,
                aliases,
                probes,
            },
            column,
        ) => {
            let inner = case_over(probes, column, order)?;
            if inner == "NULL::text" {
                None
            } else {
                let any = or_all(
                    &probes
                        .iter()
                        .map(|p| p.condition.clone())
                        .collect::<Vec<_>>(),
                );
                Some(format!(
                    "(SELECT {inner} FROM {from} WHERE {} ORDER BY {} LIMIT 1)",
                    and_all(&[where_.clone(), any]),
                    order(aliases)?
                ))
            }
        }
        (ErrorReport::SumRange, Column::Domain | Column::Left | Column::Right)
        | (ErrorReport::SumKind, Column::Domain | Column::Left | Column::Right)
        | (ErrorReport::Value { .. }, Column::Right) => None,
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

/// One contiguous run of a programme's invariants, in programme order,
/// checked by one evaluator: a run of compiled invariants is a range
/// into the compiled set, a run of interpreted ones a range into the
/// programme's invariants. The runs cover the programme in order, so an
/// execution that walks them names the first violation the kernel would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Run {
    Compiled(std::ops::Range<usize>),
    Interpreted(std::ops::Range<usize>),
}

/// What compiling a programme's invariants one by one yields: the ones
/// that compile, in programme order, the refusals for the rest, and the
/// runs that say which evaluator checks which, in order.
#[derive(Debug)]
pub(crate) struct Compilation {
    pub(crate) compiled: CompiledInvariantSet,
    pub(crate) refusals: Vec<CompileRefusal>,
    pub(crate) runs: Vec<Run>,
}

/// Compile each invariant of a validated programme that the fragment
/// holds, and say which evaluator checks each, in programme order.
pub(crate) fn compile_each(program: &ValidatedProgram<'_>) -> Compilation {
    let program = program.as_program();
    let decls: BTreeMap<&str, &PredicateDecl> = program
        .predicates
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();
    let defs: BTreeMap<&str, &Definition> = program
        .definitions
        .iter()
        .map(|d| (d.name.as_str(), d))
        .collect();
    let mut compiled = Vec::new();
    let mut refusals = Vec::new();
    let mut runs: Vec<Run> = Vec::new();
    for (index, inv) in program.invariants.iter().enumerate() {
        match compile_invariant(inv, &decls, &defs, &program.definitions) {
            Ok(c) => {
                let at = compiled.len();
                compiled.push(c);
                match runs.last_mut() {
                    Some(Run::Compiled(range)) => range.end = at + 1,
                    _ => runs.push(Run::Compiled(at..at + 1)),
                }
            }
            Err(reason) => {
                refusals.push(CompileRefusal {
                    invariant: inv.name.clone(),
                    reason,
                });
                match runs.last_mut() {
                    Some(Run::Interpreted(range)) => range.end = index + 1,
                    _ => runs.push(Run::Interpreted(index..index + 1)),
                }
            }
        }
    }
    Compilation {
        compiled: CompiledInvariantSet {
            invariants: compiled,
        },
        refusals,
        runs,
    }
}

/// Compile every invariant of a validated programme, or report every
/// refusal: nothing compiles unless everything does. What the
/// differentials ask of a whole-in-fragment programme.
#[cfg_attr(not(test), expect(dead_code, reason = "only the differentials use it"))]
pub(crate) fn compile_invariants(
    program: &ValidatedProgram<'_>,
) -> Result<CompiledInvariantSet, Vec<CompileRefusal>> {
    let compilation = compile_each(program);
    if compilation.refusals.is_empty() {
        Ok(compilation.compiled)
    } else {
        Err(compilation.refusals)
    }
}

/// What a variable stands for while a scope renders: a column of a
/// joined claim, or, inside a definition body, the literal a call bound
/// its parameter to.
#[derive(Debug, Clone)]
enum Bound {
    Col(ColRef),
    Lit(Value),
}

type Env = BTreeMap<Var, Bound>;

/// The columns an environment binds: a caller's environment holds
/// nothing else, since a call exports only the columns its body bound.
fn columns(env: &Env) -> BTreeMap<Var, ColRef> {
    env.iter()
        .filter_map(|(v, b)| match b {
            Bound::Col(col) => Some((v.clone(), col.clone())),
            Bound::Lit(_) => None,
        })
        .collect()
}

impl<'a> Ctx<'a> {
    /// A rendering context at the top of a body: outside any sum or
    /// consequent, nothing pending.
    fn fresh(
        decls: &'a BTreeMap<&'a str, &'a PredicateDecl>,
        defs: &'a BTreeMap<&'a str, &'a Definition>,
    ) -> Self {
        Ctx {
            decls,
            defs,
            in_sum: false,
            in_consequent: false,
            binding_calls: false,
            counter: 0,
            required: BTreeSet::new(),
            pending: Vec::new(),
            values: Vec::new(),
        }
    }
}

/// The prefix of the pseudo-predicate a synthetic table stands under: a
/// name no declaration can carry, so its columns register no index need.
pub(super) const CALL_TABLE_PREFIX: &str = "_call:";

struct Ctx<'a> {
    decls: &'a BTreeMap<&'a str, &'a PredicateDecl>,
    defs: &'a BTreeMap<&'a str, &'a Definition>,
    /// Rendering a sum's body, where a call's internal multiplicity
    /// would count.
    in_sum: bool,
    /// Rendering the rule's own consequent, the one place an `or`
    /// compiles: a disjunction there is a truth test over bindings
    /// already made. Cleared under `not`, `exists`, a sum and a nested
    /// implication.
    in_consequent: bool,
    /// A defined call that binds under a sum may render as the distinct
    /// projection of its body's branches: a live derived view's body,
    /// which reports no witness; an invariant's diagnosis reads claim
    /// aliases, so there it stays refused.
    binding_calls: bool,
    counter: usize,
    /// Every (predicate, position) the SQL seeks on, collected where the
    /// seek is emitted: the index specification.
    required: BTreeSet<(PredicateName, usize)>,
    /// What a comparison's operands rendered, in the kernel's order of
    /// evaluation: the comparison collects it into its own rendering.
    pending: Vec<Pending>,
    /// The computed values whose errors a `value` report names, by index.
    values: Vec<ValueExpr>,
}

/// A part of a comparison's operands its rendering must carry: a sum with
/// its own probes, a sum reported through the value it sits in, which
/// keeps only its lateral, or a computed value's probe.
enum Pending {
    Sum(RenderedSum),
    Covered(String),
    Value(Probe),
}

/// A sum inside a computed value: where its total, its range test and its
/// kind test are read, for the kernel to evaluate the value over.
#[derive(Debug, Clone)]
struct SumPart {
    lateral: String,
    total: String,
    kind: String,
    range: String,
}

/// One sum, computed once per row of the scope it sits in as a LATERAL
/// item, with the representability test of its total and the test for a
/// value it could not take.
struct RenderedSum {
    lateral: String,
    range_error: String,
    kind_error: String,
    /// Whether the sum can meet a value other than a decimal: a literal
    /// target cannot.
    can_meet_foreign: bool,
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
    /// A sum met a value other than a decimal, which the kernel decides.
    SumKind,
    /// Two operands the kernel cannot order: the tagged values, as jsonb
    /// expressions, for the kernel to judge.
    Compare {
        domain: OrderedDomain,
        left: String,
        right: String,
    },
    /// A computed value the kernel cannot evaluate on the row: the
    /// expression by index, and the bindings and sum results it reads,
    /// for the kernel to evaluate and name the error.
    Value {
        index: usize,
        vars: Vec<(String, String)>,
        sums: Vec<SumPart>,
    },
    /// A scope the kernel evaluates whole for each row of the enclosing
    /// one, with its probes in the kernel's order. The report is read from
    /// the first inner row, in the kernel's order, where any of them
    /// raises, and names the first that does there.
    Lifted {
        from: String,
        where_: String,
        aliases: Vec<(String, PredicateName)>,
        probes: Vec<Probe>,
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
    /// An `or` ended this scope: it exported no binding, so nothing may
    /// follow it in the scope and a call through it may bind no
    /// parameter.
    disjoins: bool,
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
            // One probe for the whole inner scope: the kernel meets its
            // rows in order and every way of raising on each, so the
            // first row where any raises names the first that does.
            let from = d.sources();
            let where_ = joined(&d.where_);
            lifted.probes.push(Probe {
                condition: format!(
                    "EXISTS (SELECT 1 FROM {from} WHERE {})",
                    and_all(&[where_.clone(), d.any()])
                ),
                report: ErrorReport::Lifted {
                    from,
                    where_,
                    aliases: d.aliases(),
                    probes: d.probes.clone(),
                },
            });
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
    if r.probes.iter().flat_map(|d| d.probes.iter()).any(|p| {
        matches!(
            p.report,
            ErrorReport::Compare { .. } | ErrorReport::Value { .. }
        )
    }) {
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
    defs: &BTreeMap<&str, &Definition>,
    definitions: &[Definition],
) -> Result<CompiledInvariant, CompileReason> {
    let mut ctx = Ctx::fresh(decls, defs);

    let (select_from_where, order_limit, error, case_cols, diagnosis) = match &inv.body {
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
                (
                    format!(
                        "SELECT 1 AS \"w\"\nFROM {}\nWHERE {}",
                        from_list(&r),
                        violated(&r, "true".to_string()).join("\n  AND ")
                    ),
                    format!("\nORDER BY {}\nLIMIT 1", violation_order(&r)),
                    r.probes.iter().map(ScopeDraft::anchored).collect(),
                    columns(&r.env),
                    None,
                )
            }
        }
        other => generic_denial(other, &mut ctx)?,
    };

    let plan = ImpactPlan::with_definitions(inv, definitions);
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
        plan,
        case_cols,
        sql_select_from_where: select_from_where,
        sql_order_limit: order_limit,
        probe_scopes: error,
        diagnosis,
        values: ctx.values,
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
type Denial = (
    String,
    String,
    Vec<ProbeScope>,
    BTreeMap<Var, ColRef>,
    Option<Level>,
);

fn compile_denial(left: &Prop, right: &Prop, ctx: &mut Ctx<'_>) -> Result<Denial, CompileReason> {
    let ant = render_prop(left, Env::new(), ctx)?;
    if ant.from.is_empty() {
        // Filter-only antecedent: nothing to witness, so use the generic
        // denial.
        return generic_denial_implies(left, right, ctx);
    }
    let cons = in_consequent(ctx, |ctx| render_prop(right, ant.env.clone(), ctx))?;
    if cons.closes_scope && !cons.from.is_empty() {
        return Err(CompileReason::SumShape {
            detail: "sum beside claim patterns in a consequent",
        });
    }
    // The kernel evaluates the whole consequent for each antecedent
    // match, so its scopes collapse into one scope of the antecedent
    // row, after the antecedent's own.
    let mut lifted = lift(&cons.probes);
    // The truth query already lists the antecedent's LATERAL items; the
    // consequent's own join it once more.
    let consequent_laterals = lifted.laterals.clone();
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
    scope.laterals.extend(consequent_laterals);
    if !lifted.probes.is_empty() {
        scope.probes.push(lifted);
    }
    let entry = columns(&ant.env)
        .into_iter()
        .map(|(v, c)| (v, Bound::Col(c)))
        .collect();
    let from = from_list(&scope);
    let where_ = violated(&scope, not_cons).join("\n  AND ");
    let error = scope.probes.iter().map(ScopeDraft::anchored).collect();
    let choice = Choice {
        from: from.clone(),
        where_: where_.clone(),
        aliases: ant.aliases(),
    };
    let diagnosis = in_consequent(ctx, |ctx| {
        diagnostic_level(
            &conjuncts_of(right),
            ant.env.clone(),
            entry,
            Some(choice),
            ctx,
        )
    })?;
    Ok((
        format!("SELECT 1 AS \"w\"\nFROM {from}\nWHERE {where_}"),
        format!("\nORDER BY {}\nLIMIT 1", violation_order(&ant)),
        error,
        columns(&ant.env),
        Some(diagnosis),
    ))
}

/// Any other top-level shape: the invariant holds iff the body matches at
/// all, so violation is bare non-existence. The kernel still diagnoses the
/// body, from no binding at all.
fn generic_denial(body: &Prop, ctx: &mut Ctx<'_>) -> Result<Denial, CompileReason> {
    let r = render_prop(body, Env::new(), ctx)?;
    let diagnosis = diagnostic_level(&conjuncts_of(body), Env::new(), Vec::new(), None, ctx)?;
    if !r.has_errors() {
        return Ok((
            format!("SELECT 1 AS \"w\"\nWHERE NOT {}", r.exists_sql()),
            String::new(),
            Vec::new(),
            BTreeMap::new(),
            Some(diagnosis),
        ));
    }
    // The kernel evaluates every binding of every scope, so an error
    // anywhere dominates; otherwise the body must match somewhere.
    let raises = or_all(
        &r.probes
            .iter()
            .map(|d| {
                let condition = and_all(&[joined(&d.where_), d.any()]);
                if d.from.is_empty() && d.laterals.is_empty() {
                    format!("({condition})")
                } else {
                    format!("EXISTS (SELECT 1 FROM {} WHERE {condition})", d.sources())
                }
            })
            .collect::<Vec<_>>(),
    );
    let holds = if r.from.is_empty() && r.laterals.is_empty() {
        format!("({})", and_all(&r.where_))
    } else {
        format!(
            "EXISTS (SELECT 1 FROM {} WHERE {})",
            from_list(&r),
            and_all(&r.where_)
        )
    };
    Ok((
        format!("SELECT 1 AS \"w\"\nWHERE {raises} OR NOT {holds}"),
        String::new(),
        r.probes.iter().map(ScopeDraft::anchored).collect(),
        BTreeMap::new(),
        Some(diagnosis),
    ))
}

fn generic_denial_implies(
    left: &Prop,
    right: &Prop,
    ctx: &mut Ctx<'_>,
) -> Result<Denial, CompileReason> {
    let l = render_prop(left, Env::new(), ctx)?;
    let r = in_consequent(ctx, |ctx| render_prop(right, l.env.clone(), ctx))?;
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
    let consequent_laterals = lifted.laterals.clone();
    lifted.prepend(&l);
    let mut scope = Rendered {
        laterals: l.laterals.clone(),
        where_: l.where_.clone(),
        probes: l.probes.clone(),
        ..Rendered::default()
    };
    scope.laterals.extend(consequent_laterals);
    if !lifted.probes.is_empty() {
        scope.probes.push(lifted);
    }
    let error = scope.probes.iter().map(ScopeDraft::anchored).collect();
    let items = violated(&scope, not_r.clone());
    let mut chosen_where = l.where_.clone();
    chosen_where.push(not_r);
    let choice = Choice {
        from: from_list(&l),
        where_: joined(&chosen_where),
        aliases: Vec::new(),
    };
    let entry = l.env.iter().map(|(v, b)| (v.clone(), b.clone())).collect();
    let diagnosis = in_consequent(ctx, |ctx| {
        diagnostic_level(
            &conjuncts_of(right),
            l.env.clone(),
            entry,
            Some(choice),
            ctx,
        )
    })?;
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
        BTreeMap::new(),
        Some(diagnosis),
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

/// The violation query's order. It reports nothing, so any order would
/// do; this one is kept off the primary key, whose raw order tempts the
/// planner into an early-stop scan of the whole predicate (seen at 100k
/// rows).
fn violation_order(r: &Rendered) -> String {
    let env = columns(&r.env);
    if env.is_empty() {
        r.from
            .iter()
            .map(|(alias, _, _)| format!("{alias}.arguments"))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        env.values()
            .map(|col| format!("({})::text", key_sql(col)))
            .collect::<Vec<_>>()
            .join(", ")
    }
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

/// A literal in a claim pattern: the position must hold exactly it.
fn literal_filter(col: &ColRef, value: &Value, ctx: &mut Ctx<'_>) -> Result<String, CompileReason> {
    if matches!(value, Value::CalendarSpan(_)) {
        return Err(CompileReason::Literal {
            kind: "calendar span",
        });
    }
    let ev = EvalValue::from(value);
    ctx.required.insert((col.predicate.clone(), col.position));
    const_eq_sql(col, &ev)
}

/// A literal's runtime value, as the evaluator would parse it. A literal
/// a validated programme carries always parses.
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
                if acc.disjoins {
                    return Err(CompileReason::DisjunctionShape {
                        detail: "a conjunct after an `or` in its scope",
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
                acc.disjoins = r.disjoins;
                acc.stages.push((acc.from.len(), acc.where_.len()));
            }
            Ok(acc)
        }
        Prop::Not(inner) => {
            let r = nested_scope(outside_consequent(ctx, |ctx| {
                render_prop(inner, env.clone(), ctx)
            })?)?;
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
            let r = nested_scope(outside_consequent(ctx, |ctx| {
                render_prop(body, env.clone(), ctx)
            })?)?;
            Ok(Rendered {
                where_: vec![r.exists_sql()],
                probes: lifted_scopes(&r.probes),
                env,
                ..Rendered::default()
            })
        }
        Prop::Implies { left, right } => {
            // A nested implication's consequent is not the rule's: an
            // `or` there stays outside the fragment until a workload
            // asks for it.
            let l = nested_scope(outside_consequent(ctx, |ctx| {
                render_prop(left, env.clone(), ctx)
            })?)?;
            let r = nested_scope(outside_consequent(ctx, |ctx| {
                render_prop(right, l.env.clone(), ctx)
            })?)?;
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
            // row: the consequent's plan collapses per antecedent row
            // before the antecedent's rows are joined in.
            let mut drafts = l.probes.clone();
            let mut consequent = lift(&r.probes);
            if !consequent.probes.is_empty() {
                consequent.prepend(&l);
                drafts.push(consequent);
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
            OrderedDomain::Decimal | OrderedDomain::Timestamp | OrderedDomain::Date => {
                ordered_sql(*op, *domain, left, right, &env, ctx)
            }
            OrderedDomain::Duration => Err(CompileReason::ComparisonDomain { domain: *domain }),
        },
        Prop::Or(branches) => {
            if !ctx.in_consequent {
                if let Some(test) = exclusive_literal_test(branches, &env, ctx)? {
                    return Ok(Rendered {
                        where_: vec![test],
                        env,
                        ..Rendered::default()
                    });
                }
                return Err(CompileReason::Construct { construct: "or" });
            }
            // Each branch is its own scope under the bindings on entry;
            // the disjunction is their truth joined, exporting nothing.
            // The kernel evaluates every branch, so every branch's
            // raising scopes lift, in branch order.
            let mut tests = Vec::new();
            let mut drafts = Vec::new();
            for branch in branches {
                let r = nested_scope(render_prop(branch, env.clone(), ctx)?)?;
                tests.push(if !r.from.is_empty() {
                    r.exists_sql()
                } else if r.has_errors() {
                    format!("({})", and_all(&r.where_))
                } else {
                    format!("({})", r.conjunction())
                });
                drafts.extend(r.probes);
            }
            Ok(Rendered {
                where_: vec![format!("({})", tests.join(" OR "))],
                probes: lifted_scopes(&drafts),
                env,
                disjoins: true,
                ..Rendered::default()
            })
        }
        Prop::Xor(_, _) => Err(CompileReason::Construct { construct: "xor" }),
        Prop::Pre(_) => Err(CompileReason::Construct { construct: "pre" }),
        Prop::Defined { name, args } => render_defined(name, args, env, ctx),
        Prop::In(_, _) => Err(CompileReason::Construct { construct: "in" }),
    }
}

/// A call to a definition. The body renders under the call's frame
/// alone: a parameter bound to a caller column or a literal, an unbound
/// one free for the body to bind. Its joins and conjuncts join the
/// caller's chain for truth and bindings; only the parameters project
/// back to the caller (a bound one already agreed, an unbound one binds
/// its argument, a repeated argument must agree, a wildcard receives
/// nothing), so the body's own variables never escape. The kernel
/// evaluates the body whole for each caller binding, so its error plan
/// is a second rendering under fresh aliases, lifted per caller row.
/// A call's definition and the frame its body renders under: each
/// parameter bound to what its argument is bound to, a wildcard or a
/// variable not yet bound leaving it free.
/// The condition a called definition names; a figure here is unvalidated
/// IR, since the checker refuses a figure called as a condition.
fn condition_body(def: &Definition) -> Result<&Prop, CompileReason> {
    def.body
        .as_prop()
        .ok_or_else(|| CompileReason::UnvalidatedShape {
            detail: format!("{} names a figure, not a condition", def.name),
        })
}

/// The figure a called definition names, rendered under the frame its
/// arguments give: every argument must be a term, since an expression
/// has no column to bind a parameter to.
fn value_call_frame<'a>(
    name: &DefinitionName,
    args: &[ValueExpr],
    env: &Env,
    ctx: &Ctx<'a>,
) -> Result<(&'a ValueExpr, Env), CompileReason> {
    let terms: Vec<Term> = args
        .iter()
        .map(|a| match a {
            ValueExpr::Term(t) => Ok(t.clone()),
            _ => Err(CompileReason::Construct {
                construct: "a figure called with an expression argument",
            }),
        })
        .collect::<Result<_, _>>()?;
    let (def, frame) = call_frame(name, &terms, env, ctx)?;
    let body = def
        .body
        .as_value()
        .ok_or_else(|| CompileReason::UnvalidatedShape {
            detail: format!("{} names a condition, not a figure", def.name),
        })?;
    Ok((body, frame))
}

fn call_frame<'a>(
    name: &DefinitionName,
    args: &[Term],
    env: &Env,
    ctx: &Ctx<'a>,
) -> Result<(&'a Definition, Env), CompileReason> {
    let def = *ctx
        .defs
        .get(name.as_str())
        .ok_or_else(|| CompileReason::UnvalidatedShape {
            detail: format!("undeclared definition {name}"),
        })?;
    if def.parameters.len() != args.len() {
        return Err(CompileReason::UnvalidatedShape {
            detail: format!("arity mismatch on {name}"),
        });
    }
    // Every call is rendered by inlining its body, which a body that
    // calls itself has no end to.
    if def.calls_itself() {
        return Err(CompileReason::Construct {
            construct: "a definition that calls itself",
        });
    }
    let mut frame = Env::new();
    for (param, arg) in def.parameters.iter().zip(args) {
        match arg {
            Term::Wildcard => {}
            Term::Actor => return Err(CompileReason::Construct { construct: "actor" }),
            Term::DecidedAt => {
                return Err(CompileReason::Construct {
                    construct: "decided_at",
                });
            }
            Term::Literal(value) => {
                frame.insert(param.clone(), Bound::Lit(value.clone()));
            }
            Term::Var(x) => {
                if let Some(bound) = env.get(x) {
                    frame.insert(param.clone(), bound.clone());
                }
            }
        }
    }
    Ok((def, frame))
}

fn render_defined(
    name: &DefinitionName,
    args: &[Term],
    env: Env,
    ctx: &mut Ctx<'_>,
) -> Result<Rendered, CompileReason> {
    if ctx.in_sum {
        // A sum observes how many witnesses a call has, and the kernel
        // yields each distinct projection once. With every argument
        // bound there is one projection, so the call is a truth test and
        // counts each outer witness once, as the kernel does. A call that
        // binds has as many projections as distinct bindings, which a
        // join does not reproduce.
        if args
            .iter()
            .any(|arg| matches!(arg, Term::Var(x) if !env.contains_key(x)))
        {
            if ctx.binding_calls {
                return render_binding_call(name, args, env, ctx);
            }
            return Err(CompileReason::SumShape {
                detail: "a defined call under a sum that binds a parameter",
            });
        }
        let (def, frame) = call_frame(name, args, &env, ctx)?;
        let body = condition_body(def)?;
        let r = nested_scope(outside_consequent(ctx, |ctx| {
            render_prop(body, frame, ctx)
        })?)?;
        return Ok(Rendered {
            where_: vec![r.exists_sql()],
            probes: lifted_scopes(&r.probes),
            env,
            ..Rendered::default()
        });
    }
    let (def, frame) = call_frame(name, args, &env, ctx)?;
    let body = condition_body(def)?;
    let truth = render_prop(body, frame.clone(), ctx)?;
    let mut env = env;
    let mut where_ = truth.where_.clone();
    for (param, arg) in def.parameters.iter().zip(args) {
        let Term::Var(x) = arg else {
            continue;
        };
        if frame.contains_key(param) {
            continue;
        }
        if truth.disjoins {
            // The kernel would bind the parameter from whichever branch
            // matched; the SQL test keeps the branches' bindings inside.
            return Err(CompileReason::DisjunctionShape {
                detail: "a call whose `or` would bind a parameter",
            });
        }
        let Some(Bound::Col(generated)) = truth.env.get(param) else {
            return Err(CompileReason::UnvalidatedShape {
                detail: format!("{name} never binds its parameter {param}"),
            });
        };
        match env.get(x) {
            Some(Bound::Col(earlier)) => {
                where_.push(col_eq(earlier, generated));
                ctx.required
                    .insert((earlier.predicate.clone(), earlier.position));
                ctx.required
                    .insert((generated.predicate.clone(), generated.position));
            }
            Some(Bound::Lit(_)) => {
                return Err(CompileReason::UnvalidatedShape {
                    detail: format!("{x} is bound to a literal outside a call"),
                });
            }
            None => {
                env.insert(x.clone(), Bound::Col(generated.clone()));
            }
        }
    }
    if truth.closes_scope {
        return Err(CompileReason::SumShape {
            detail: "a sum inside a defined call",
        });
    }
    let probes = if truth.has_errors() {
        let again = render_prop(body, frame, ctx)?;
        let mut scope = lift(&again.probes);
        // The kernel reaches a body's raising expression only through the
        // body's joins before it, so the lifted scope sits past those in
        // the caller's chain: a probe branch over the caller row alone
        // would leave the inlined joins unconstrained.
        let earliest = &truth.probes[0];
        scope.from = earliest.from.clone();
        scope.laterals = earliest.laterals.clone();
        scope.where_ = earliest.where_.clone();
        vec![scope]
    } else {
        Vec::new()
    };
    Ok(Rendered {
        from: truth.from,
        laterals: truth.laterals,
        where_,
        probes,
        env,
        closes_scope: truth.closes_scope,
        disjoins: truth.disjoins,
        stages: Vec::new(),
    })
}

/// An `or` of equalities of one bound variable against distinct literals,
/// the shape `x in (a, b)` desugars to, as one SQL test. The kernel keeps
/// every branch's matches, so an `or` whose branches can hold together
/// counts a witness once per holding branch, where a SQL `OR` counts it
/// once; equalities of one variable against distinct literals hold one
/// at a time, so the two agree. Anything else is not this shape.
fn exclusive_literal_test(
    branches: &[Prop],
    env: &Env,
    ctx: &mut Ctx<'_>,
) -> Result<Option<String>, CompileReason> {
    let mut variable: Option<&Var> = None;
    let mut literals: Vec<EvalValue> = Vec::new();
    let mut tests = Vec::new();
    for branch in branches {
        let Prop::Eq(a, b) = branch else {
            return Ok(None);
        };
        let ((ValueExpr::Term(Term::Var(x)), ValueExpr::Term(Term::Literal(value)))
        | (ValueExpr::Term(Term::Literal(value)), ValueExpr::Term(Term::Var(x)))) =
            (a.as_ref(), b.as_ref())
        else {
            return Ok(None);
        };
        if variable.is_some_and(|seen| seen != x) || !env.contains_key(x) {
            return Ok(None);
        }
        variable = Some(x);
        let value = EvalValue::from(value);
        if literals.contains(&value) {
            return Err(CompileReason::DisjunctionShape {
                detail: "a literal repeated in an `or` of equalities, which the kernel counts twice",
            });
        }
        literals.push(value);
        let rendered = equality_sql(a, b, false, env, ctx)?;
        tests.push(rendered.conjunction());
    }
    Ok(Some(format!("({})", tests.join(" OR "))))
}

/// A defined call under a sum that binds a parameter, as the kernel
/// counts it: each distinct projection of the call's arguments once. The
/// body's branches (the disjuncts of an `or`, or the body itself) each
/// render under the call frame and project the parameters as one tagged
/// array; their `UNION ALL` is the kernel's concatenation of matches, and
/// `DISTINCT ON` the projection's keys is its deduplication, by the
/// kernel's equality. The unbound arguments then read the synthetic
/// table's columns as a claim's. A branch that can raise refuses, as any
/// raising scope under a sum does.
fn render_binding_call(
    name: &DefinitionName,
    args: &[Term],
    env: Env,
    ctx: &mut Ctx<'_>,
) -> Result<Rendered, CompileReason> {
    let (def, frame) = call_frame(name, args, &env, ctx)?;
    let branches: Vec<&Prop> = match condition_body(def)? {
        Prop::Or(branches) => branches.iter().collect(),
        body => vec![body],
    };
    let mut selects = Vec::with_capacity(branches.len());
    let mut kinds: Vec<Option<PredicateArgKind>> = vec![None; def.parameters.len()];
    for branch in branches {
        let r = nested_scope(outside_consequent(ctx, |ctx| {
            render_prop(branch, frame.clone(), ctx)
        })?)?;
        if r.has_errors() {
            return Err(CompileReason::SumShape {
                detail: "a defined call under a sum whose body can raise",
            });
        }
        if r.from.is_empty() {
            return Err(CompileReason::SumShape {
                detail: "a defined call under a sum binding from no claim",
            });
        }
        let mut parts = Vec::with_capacity(def.parameters.len());
        for (i, param) in def.parameters.iter().enumerate() {
            match r.env.get(param) {
                Some(Bound::Col(col)) => {
                    kinds[i].get_or_insert_with(|| col.kind.clone());
                    parts.push(tagged_sql(col));
                }
                Some(Bound::Lit(value)) => {
                    let ev = EvalValue::from(value);
                    kinds[i].get_or_insert_with(|| ev.kind());
                    parts.push(format!("{}::jsonb", quote_literal(&tagged_json(&ev)?)));
                }
                None => {
                    return Err(CompileReason::UnvalidatedShape {
                        detail: format!("{name} binds no parameter {param} in a branch"),
                    });
                }
            }
        }
        selects.push(format!(
            "SELECT jsonb_build_array({}) AS arguments FROM {} WHERE {}",
            parts.join(", "),
            from_list(&r),
            r.conjunction()
        ));
    }
    let alias = format!("c{}", ctx.counter);
    ctx.counter += 1;
    let pseudo = PredicateName::from(format!("{CALL_TABLE_PREFIX}{name}"));
    let seeks: Vec<String> = (0..def.parameters.len())
        .map(|i| seek_expression("u.", i))
        .collect();
    let item = format!(
        "LATERAL (SELECT DISTINCT ON ({seeks}) u.arguments FROM ({union}) u ORDER BY {seeks}) {alias}",
        seeks = seeks.join(", "),
        union = selects.join(" UNION ALL ")
    );
    let mut env = env;
    let mut seen: BTreeSet<&Var> = BTreeSet::new();
    for (i, arg) in args.iter().enumerate() {
        let Term::Var(x) = arg else {
            continue;
        };
        if env.contains_key(x) {
            continue;
        }
        if !seen.insert(x) {
            return Err(CompileReason::UnvalidatedShape {
                detail: format!("{name} called with {x} twice unbound"),
            });
        }
        let kind = kinds[i]
            .clone()
            .ok_or_else(|| CompileReason::UnvalidatedShape {
                detail: format!("{name} binds {x} to no known kind"),
            })?;
        env.insert(
            x.clone(),
            Bound::Col(ColRef {
                alias: alias.clone(),
                predicate: pseudo.clone(),
                position: i,
                kind,
            }),
        );
    }
    Ok(Rendered {
        from: vec![(alias, pseudo, item)],
        env,
        ..Rendered::default()
    })
}

/// Render under the consequent position, where an `or` compiles.
fn in_consequent<T>(ctx: &mut Ctx<'_>, f: impl FnOnce(&mut Ctx<'_>) -> T) -> T {
    let outside = std::mem::replace(&mut ctx.in_consequent, true);
    let out = f(ctx);
    ctx.in_consequent = outside;
    out
}

/// Render a nested scope that is not a consequent: an `or` there stays
/// outside the fragment.
fn outside_consequent<T>(ctx: &mut Ctx<'_>, f: impl FnOnce(&mut Ctx<'_>) -> T) -> T {
    let outside = std::mem::replace(&mut ctx.in_consequent, false);
    let out = f(ctx);
    ctx.in_consequent = outside;
    out
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
            Term::DecidedAt => {
                return Err(CompileReason::Construct {
                    construct: "decided_at",
                });
            }
            Term::Literal(v) => {
                where_.push(literal_filter(&col, v, ctx)?);
            }
            Term::Var(v) => match env.get(v) {
                Some(Bound::Col(bound)) => {
                    where_.push(col_eq(bound, &col));
                    ctx.required
                        .insert((bound.predicate.clone(), bound.position));
                    ctx.required.insert((predicate.clone(), i));
                }
                Some(Bound::Lit(value)) => {
                    where_.push(literal_filter(&col, value, ctx)?);
                }
                None => {
                    env.insert(v.clone(), Bound::Col(col));
                }
            },
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
    /// For a decimal or a quantity: its amount and the unit its value
    /// carries (NULL for a bare decimal), read from the value itself.
    dd: Option<(String, String)>,
    /// The unit, when it is known before any value is read: `Some(None)`
    /// for a bare decimal literal or a sum, `Some(Some(unit))` for a
    /// quantity literal, `None` for a value read at run time.
    static_unit: Option<Option<String>>,
}

/// How a value takes part in an ordered comparison, with the test that
/// it is of the kind the comparison expects and its tagged form for the
/// kernel to word an error with. `well_typed` is `true` for a literal or
/// a total. A decimal or a quantity is ordered by the amount and unit its
/// value carries (`Operand::dd`), so these two keep only what equality
/// and the error report read.
enum Ordered {
    Numeric {
        sql: String,
        tagged: String,
    },
    Quantity {
        tagged: String,
    },
    Timestamp {
        nanos: String,
        well_typed: String,
        tagged: String,
    },
    Date {
        ordinal: String,
        well_typed: String,
        tagged: String,
    },
    /// A kind nothing orders here, or one the compiler cannot tell at
    /// compile time: the declared kind, for a column.
    None(Option<PredicateArgKind>),
}

impl Ordered {
    /// The value's tagged form, for the kernel to word an error with.
    fn tagged(&self) -> Option<&str> {
        match self {
            Ordered::Numeric { tagged, .. }
            | Ordered::Quantity { tagged, .. }
            | Ordered::Timestamp { tagged, .. }
            | Ordered::Date { tagged, .. } => Some(tagged),
            Ordered::None(_) => None,
        }
    }
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
    // Decimals and quantities compare by their values' flavour, not the
    // declaration's: an amount and the unit each value carries, ordered
    // only where the kernel would order them. What a literal or a sum
    // fixes before any value is read needs no test.
    if domain == OrderedDomain::Decimal
        && let (Some((xa, xu)), Some((ya, yu)), Some(lt), Some(rt)) =
            (&l.dd, &r.dd, l.ordered.tagged(), r.ordered.tagged())
    {
        let mut tests = Vec::new();
        if l.static_unit.is_none() {
            tests.push(format!("({xa}) IS NOT NULL"));
        }
        if r.static_unit.is_none() {
            tests.push(format!("({ya}) IS NOT NULL"));
        }
        match (&l.static_unit, &r.static_unit) {
            (Some(a), Some(b)) => {
                if a != b {
                    tests.push("false".to_string());
                }
            }
            _ => tests.push(format!("({xu}) IS NOT DISTINCT FROM ({yu})")),
        }
        let clause = format!("({xa}) {op} ({ya})");
        let raises = raising(and_all(&tests), domain, lt, rt);
        return Ok(close_comparison(clause, raises, env, ctx));
    }
    let (clause, raises) = match (domain, &l.ordered, &r.ordered) {
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
        (
            OrderedDomain::Date,
            Ordered::Date {
                ordinal: xo,
                well_typed: xw,
                tagged: xt,
            },
            Ordered::Date {
                ordinal: yo,
                well_typed: yw,
                tagged: yt,
            },
        ) => (
            format!("({xo}) {op} ({yo})"),
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
    let pending = std::mem::take(&mut ctx.pending);
    let mut probes: Vec<Probe> = Vec::new();
    let mut laterals: Vec<String> = Vec::new();
    for p in pending {
        match p {
            Pending::Sum(s) => {
                if s.can_meet_foreign {
                    probes.push(Probe {
                        condition: s.kind_error.clone(),
                        report: ErrorReport::SumKind,
                    });
                }
                probes.push(Probe {
                    condition: s.range_error.clone(),
                    report: ErrorReport::SumRange,
                });
                laterals.push(s.lateral);
            }
            Pending::Covered(lateral) => laterals.push(lateral),
            Pending::Value(probe) => probes.push(probe),
        }
    }
    probes.extend(raises);
    if probes.is_empty() {
        return Rendered {
            where_: vec![clause],
            env: env.clone(),
            ..Rendered::default()
        };
    }
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
            let col = match env.get(v) {
                Some(Bound::Col(col)) => col,
                Some(Bound::Lit(value)) => return literal_operand(value),
                None => {
                    return Err(CompileReason::UnvalidatedShape {
                        detail: format!("unbound variable in value position: {v}"),
                    });
                }
            };
            let ordered = match &col.kind {
                PredicateArgKind::Decimal => Ordered::Numeric {
                    sql: decimal_sql(col),
                    tagged: tagged_sql(col),
                },
                PredicateArgKind::Quantity(_) => Ordered::Quantity {
                    tagged: tagged_sql(col),
                },
                PredicateArgKind::Timestamp => Ordered::Timestamp {
                    nanos: format!("morpholog.timestamp_nanos({})", tagged_sql(col)),
                    well_typed: tagged_as_sql(col, "timestamp"),
                    tagged: tagged_sql(col),
                },
                PredicateArgKind::Date => Ordered::Date {
                    ordinal: format!("morpholog.date_ordinal({})", tagged_sql(col)),
                    well_typed: tagged_as_sql(col, "date"),
                    tagged: tagged_sql(col),
                },
                other => Ordered::None(Some(other.clone())),
            };
            Ok(Operand {
                key: Some(key_sql(col)),
                ordered,
                dd: arith::column_dd(col),
                static_unit: None,
            })
        }
        ValueExpr::Term(Term::Literal(v)) => literal_operand(v),
        ValueExpr::Term(Term::Wildcard) => Err(CompileReason::Construct {
            construct: "wildcard value",
        }),
        ValueExpr::Term(Term::Actor) => Err(CompileReason::Construct { construct: "actor" }),
        ValueExpr::Term(Term::DecidedAt) => Err(CompileReason::Construct {
            construct: "decided_at",
        }),
        ValueExpr::Sum { value, body, seed } => {
            if *seed != SumSeed::Decimal {
                return Err(CompileReason::SumShape {
                    detail: "non-decimal seed",
                });
            }
            let outside = std::mem::replace(&mut ctx.in_sum, true);
            let r = outside_consequent(ctx, |ctx| render_prop(body, env.clone(), ctx));
            ctx.in_sum = outside;
            let r = r?;
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
            let (val, kind_test, can_meet_foreign): (String, String, bool) = match value.as_ref() {
                ValueExpr::Term(Term::Var(v)) => {
                    let Some(Bound::Col(col)) = r.env.get(v) else {
                        return Err(CompileReason::UnvalidatedShape {
                            detail: format!("sum target unbound: {v}"),
                        });
                    };
                    if col.kind != PredicateArgKind::Decimal {
                        return Err(CompileReason::SumShape {
                            detail: "target is not a decimal position",
                        });
                    }
                    (
                        decimal_sql(col),
                        format!(
                            "bool_or(({}.arguments -> {} ->> 'type') IS DISTINCT FROM 'decimal')",
                            col.alias, col.position
                        ),
                        true,
                    )
                }
                ValueExpr::Term(Term::Literal(Value::Decimal(d))) => (
                    format!("{}::numeric", quote_literal(&d.to_string())),
                    "false".to_string(),
                    false,
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
            ctx.pending.push(Pending::Sum(RenderedSum {
                lateral: format!(
                    "LATERAL (SELECT COALESCE(sum({val}), 0::numeric) AS s, COALESCE({kind_test}, false) AS f FROM {} WHERE {}) {alias}",
                    from_list(&r),
                    r.conjunction()
                ),
                range_error: range_error_sql(&total),
                kind_error: format!("{alias}.f"),
                can_meet_foreign,
            }));
            Ok(Operand {
                key: None,
                ordered: Ordered::Numeric {
                    sql: total.clone(),
                    tagged: format!(
                        "jsonb_build_object('type', 'decimal', 'value', ({total})::text)"
                    ),
                },
                dd: Some((total, "NULL::text".to_string())),
                static_unit: Some(None),
            })
        }
        ValueExpr::Arith { .. } | ValueExpr::Call { .. } => arith::computed_operand(expr, env, ctx),
        ValueExpr::Defined { name, args } => {
            let (body, frame) = value_call_frame(name, args, env, ctx)?;
            value_sql(body, &frame, ctx)
        }
        ValueExpr::Extremum { .. } => Err(CompileReason::Construct {
            construct: "extremum",
        }),
        ValueExpr::ValueOf { .. } => Err(CompileReason::Construct {
            construct: "value lookup",
        }),
        ValueExpr::Cond { .. } => Err(CompileReason::Construct {
            construct: "conditional value",
        }),
    }
}

/// The coordinate `morpholog.date_ordinal` gives a stored date, for a
/// literal: monotone in the civil fields, true to negative years.
fn date_ordinal(d: jiff::civil::Date) -> i64 {
    i64::from(d.year()) * 10000 + i64::from(d.month()) * 100 + i64::from(d.day())
}

/// A literal in value position: its key, and a SQL constant of the
/// kernel's value for ordering. A timestamp's or a date's coordinate is
/// computed here, not by the database.
fn literal_operand(value: &Value) -> Result<Operand, CompileReason> {
    if matches!(value, Value::CalendarSpan(_)) {
        return Err(CompileReason::Literal {
            kind: "calendar span",
        });
    }
    let ev = EvalValue::from(value);
    let tagged = format!("{}::jsonb", quote_literal(&tagged_json(&ev)?));
    let ordered = match &ev {
        EvalValue::Decimal(d) => Ordered::Numeric {
            sql: format!("{}::numeric", quote_literal(&d.to_string())),
            tagged,
        },
        EvalValue::Quantity { .. } => Ordered::Quantity { tagged },
        EvalValue::Timestamp(t) => Ordered::Timestamp {
            nanos: format!("{}::numeric", t.as_nanosecond()),
            well_typed: "true".to_string(),
            tagged,
        },
        EvalValue::Date(d) => Ordered::Date {
            ordinal: format!("{}::integer", date_ordinal(*d)),
            well_typed: "true".to_string(),
            tagged,
        },
        _ => Ordered::None(None),
    };
    Ok(Operand {
        key: Some(key_lit(&ev)?),
        ordered,
        dd: arith::literal_dd(value),
        static_unit: match value {
            Value::Decimal(_) => Some(None),
            Value::Quantity { unit, .. } => Some(Some(unit.as_str().to_string())),
            _ => None,
        },
    })
}

mod arith;
mod live;
pub(crate) use live::{LIVE_METADATA, compile_derived};

#[cfg(test)]
mod tests;
