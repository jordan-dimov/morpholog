//! `morpholog generate views` - render a programme's predicates as a
//! typed, read-only SQL view surface: BASE predicates over
//! `morpholog.claims`, DERIVED predicates over the `morpholog_read` cache
//! that `morpholog refresh derived` populates.
//!
//! Plain relational SQL that any BI tool can read: the read-side
//! counterpart of `morpholog schema`. The renderer is **pure** (no
//! database): the same inputs give byte-identical output, so drift is
//! caught by regenerating and diffing.
//!
//! The JSONB shape comes from `EvalValue` in `morpholog-core/src/state.rs`
//! (`#[serde(tag="type", content="value")]`); the extractors here mirror it
//! position for position.
//!
//! What makes the output a read *contract*:
//!   1. **Read-only** - each view wraps its source in a top-level `WITH`,
//!      so PostgreSQL refuses `INSERT`/`UPDATE`/`DELETE` through it.
//!   2. **Atomic** - the script is one `BEGIN; ... COMMIT;`, so a failure
//!      leaves no half-updated surface.
//!   3. **Metadata columns first**, plus the raw `_morpholog_arguments`, so
//!      appending a field stays a compatible `CREATE OR REPLACE VIEW` and
//!      the exact value is always behind the typed columns.
//!   4. **Hash-pinned** - a `_morpholog_catalog` view carries the model hash
//!      and the view inventory.
//!
//! Refusal is whole-run: every bad identifier across base and derived
//! predicates is collected first, and any finding fails the run with
//! nothing written. A derived view shows only the active generation built
//! from the same model hash, so it is empty until `morpholog refresh
//! derived` runs for this programme.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

use morpholog_core::{PredicateArgKind, PredicateDecl, ValidatedProgram};

use crate::sql_quote::{quote_ident, quote_literal};

/// The rendered script plus how many views of each kind it emits,
/// excluding the catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedViews {
    pub sql: String,
    /// Views over `morpholog.claims` (one per non-derived predicate).
    pub base_view_count: usize,
    /// Views over the `morpholog_read` cache (one per derived head).
    pub derived_view_count: usize,
}

/// One reason a programme cannot be rendered as views. All are collected,
/// so the author sees the complete list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewRefusal {
    /// A field name is not a safe unquoted SQL identifier
    /// (`[a-z_][a-z0-9_]*`). PostgreSQL folds unquoted identifiers to
    /// lowercase, so an uppercase name would silently diverge from the
    /// declaration.
    InvalidIdentifier { owner: String, name: String },
    /// An identifier exceeds PostgreSQL's 63-byte limit and would be
    /// silently truncated, possibly colliding.
    IdentifierTooLong { owner: String, name: String },
    /// A business field name starts with the generator-owned
    /// `_morpholog_` prefix, which would shadow a metadata column.
    ReservedPrefix { owner: String, name: String },
    /// Two predicates (base or derived) whose names render to the same
    /// snake_case view. Both are named.
    ViewNameCollision {
        generated: String,
        sources: Vec<String>,
    },
    /// The `--schema` value is not a safe unquoted identifier, or is
    /// over-long.
    InvalidSchema { schema: String },
}

impl std::fmt::Display for ViewRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ViewRefusal::InvalidIdentifier { owner, name } => write!(
                f,
                "{owner}: `{name}` is not a safe unquoted SQL identifier \
                 (lowercase letters, digits, and underscores; not starting with a digit)"
            ),
            ViewRefusal::IdentifierTooLong { owner, name } => write!(
                f,
                "{owner}: `{name}` exceeds PostgreSQL's 63-byte identifier limit; rename it"
            ),
            ViewRefusal::ReservedPrefix { owner, name } => write!(
                f,
                "{owner}: `{name}` uses the reserved `_morpholog_` prefix; rename it"
            ),
            ViewRefusal::ViewNameCollision { generated, sources } => write!(
                f,
                "predicates {} all generate view `{generated}`; rename one",
                sources
                    .iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
            ViewRefusal::InvalidSchema { schema } => write!(
                f,
                "--schema `{schema}` is not a safe unquoted SQL identifier within \
                 PostgreSQL's 63-byte limit"
            ),
        }
    }
}

/// The generator-owned column prefix. The whole prefix is reserved, so a
/// business field may still be called `predicate_name` or `asserted_at`.
const MORPHOLOG_PREFIX: &str = "_morpholog_";

/// PostgreSQL's identifier length limit (`NAMEDATALEN - 1`). Names beyond
/// this are silently truncated.
const MAX_IDENT_BYTES: usize = 63;

/// The catalogue view's name. Carries the model hash and the intended
/// view inventory for drift-checking.
pub(crate) const CATALOG_VIEW: &str = "_morpholog_catalog";

/// The seal table: each generated view's stored definition, hashed at
/// apply time. A table, not a view: it records an observation and is not
/// part of the surface.
pub const VIEW_DEFS_TABLE: &str = "_morpholog_view_defs";

/// The SQL `SELECT` expression and the kind note for one declared
/// argument position. The expression is over the CTE-bound `arguments`
/// JSONB array; `kind_comment` becomes a persistent `COMMENT ON COLUMN`.
struct ColumnSql {
    expr: String,
    kind_comment: String,
}

/// The PG type + extractor for one argument kind at positional index `i`.
///
/// **No `_` arm**: a new [`PredicateArgKind`] must fail to compile here
/// rather than project silently wrong. Every kind is representable:
/// `Collection` and `Any` fall back to plain `jsonb` (the Python client
/// refuses them, but a read projection can carry them).
fn column_sql(kind: &PredicateArgKind, i: usize) -> ColumnSql {
    let col = |expr: String, kind_comment: String| ColumnSql { expr, kind_comment };
    match kind {
        PredicateArgKind::Subject => col(
            format!("arguments -> {i} ->> 'value'"),
            "Morpholog kind Subject".to_string(),
        ),
        PredicateArgKind::Decimal => col(
            format!("(arguments -> {i} ->> 'value')::numeric"),
            "Morpholog kind Decimal".to_string(),
        ),
        PredicateArgKind::Date => col(
            format!("(arguments -> {i} ->> 'value')::date"),
            "Morpholog kind Date".to_string(),
        ),
        PredicateArgKind::Timestamp => col(
            format!("(arguments -> {i} ->> 'value')::timestamptz"),
            "Morpholog kind Timestamp; PostgreSQL microsecond precision \
             (exact source in _morpholog_arguments)"
                .to_string(),
        ),
        PredicateArgKind::Duration => col(
            // jiff writes a negative span with a LEADING sign (`-PT6H`),
            // which PostgreSQL rejects. Strip it and negate, so one negative
            // duration does not break the whole view. Sub-microsecond parts
            // still truncate.
            format!(
                "CASE WHEN (arguments -> {i} ->> 'value') LIKE '-%' \
                 THEN -((substring(arguments -> {i} ->> 'value' FROM 2))::interval) \
                 ELSE (arguments -> {i} ->> 'value')::interval END"
            ),
            "Morpholog kind Duration; PostgreSQL microsecond precision \
             (exact source in _morpholog_arguments)"
                .to_string(),
        ),
        PredicateArgKind::Bool => col(
            format!("(arguments -> {i} ->> 'value')::boolean"),
            "Morpholog kind Bool".to_string(),
        ),
        PredicateArgKind::Quantity(unit) => col(
            format!("(arguments -> {i} -> 'value' ->> 'amount')::numeric"),
            format!("Morpholog kind Decimal[{unit}]; amount in {unit}"),
        ),
        PredicateArgKind::Collection => col(
            format!("arguments -> {i} -> 'value'"),
            "Morpholog kind Collection (jsonb array of tagged values)".to_string(),
        ),
        // Validation refuses this kind in declarations, so no column asks
        // for it; plain jsonb is the safe fallback.
        PredicateArgKind::CalendarSpan => col(
            format!("arguments -> {i}"),
            "Morpholog kind CalendarSpan (expression-only; never admitted)".to_string(),
        ),
        PredicateArgKind::Any => col(
            format!("arguments -> {i}"),
            "Morpholog kind Any (the whole tagged {type,value} object)".to_string(),
        ),
    }
}

/// `TradeSettled` -> `trade_settled`, acronym-aware: `PPAContract` ->
/// `ppa_contract`, `TradeID` -> `trade_id`, `HTTP2Request` ->
/// `http2_request`.
fn snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            let next = chars.get(i + 1).copied();
            let boundary = match prev {
                None => false,
                Some(p) => {
                    (p.is_ascii_lowercase() || p.is_ascii_digit())
                        || (p.is_ascii_uppercase() && next.is_some_and(|n| n.is_ascii_lowercase()))
                }
            };
            if boundary && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// True when `s` is a safe unquoted lowercase SQL identifier:
/// `[a-z_][a-z0-9_]*`. Length is checked separately.
fn is_safe_lower_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c == '_' => {}
        _ => return false,
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Push every identifier refusal for `name` under `owner`, for field and
/// view names alike. Only what quoting cannot rescue is refused: unsafe or
/// non-lowercase names, over-long ones, and the `_morpholog_` prefix.
fn check_identifier(owner: String, name: &str, refusals: &mut Vec<ViewRefusal>) {
    if name.starts_with(MORPHOLOG_PREFIX) {
        refusals.push(ViewRefusal::ReservedPrefix {
            owner,
            name: name.to_string(),
        });
        return;
    }
    if !is_safe_lower_ident(name) {
        refusals.push(ViewRefusal::InvalidIdentifier {
            owner,
            name: name.to_string(),
        });
        return;
    }
    if name.len() > MAX_IDENT_BYTES {
        refusals.push(ViewRefusal::IdentifierTooLong {
            owner,
            name: name.to_string(),
        });
    }
    // Reserved words (`limit`, `order`, `user`) are allowed: every
    // generated identifier is quoted. Readers must quote them too
    // (`SELECT "limit" ...`), a fair price for not banning common names.
}

/// Make a value safe inside a `--` line comment by escaping CR and LF. A
/// newline would end the comment and inject SQL; `render_views` takes an
/// arbitrary `model_hash` and a programme name nothing SQL-validates.
fn comment_text(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// Which state a view projects: base predicates read `morpholog.claims`;
/// derived predicates read the `morpholog_read` cache. The columns are the
/// same; only the source and the metadata columns differ.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewKind {
    Base,
    Derived,
}

impl ViewKind {
    /// The `kind` value the catalogue records for this view.
    fn label(self) -> &'static str {
        match self {
            ViewKind::Base => "base",
            ViewKind::Derived => "derived",
        }
    }
}

/// Render the atomic `CREATE VIEW` script for `program` into `schema`: one
/// typed view per base predicate, one per derived predicate, and the
/// `_morpholog_catalog` inventory. Pure: identical inputs give
/// byte-identical output. On any refusal, returns every finding and
/// renders nothing.
pub fn render_views(
    program: ValidatedProgram<'_>,
    schema: &str,
    model_hash: &str,
) -> Result<RenderedViews, Vec<ViewRefusal>> {
    let program = program.as_program();

    // A derived head is a declared predicate, so its declared kinds drive
    // the columns exactly as a base predicate's do.
    let derived_heads: HashSet<&str> = program
        .derived_claims
        .iter()
        .map(|d| d.predicate.as_str())
        .collect();
    let base: Vec<&PredicateDecl> = program
        .predicates
        .iter()
        .filter(|p| !derived_heads.contains(p.name.as_str()))
        .collect();
    let derived: Vec<&PredicateDecl> = program
        .predicates
        .iter()
        .filter(|p| derived_heads.contains(p.name.as_str()))
        .collect();

    let refusals = sweep(schema, &base, &derived);
    if !refusals.is_empty() {
        return Err(refusals);
    }

    let sql = render(program.name.as_str(), schema, model_hash, &base, &derived);
    Ok(RenderedViews {
        sql,
        base_view_count: base.len(),
        derived_view_count: derived.len(),
    })
}

/// Collect every reason the programme cannot be rendered, across base and
/// derived predicates. They share one schema, so the view-name collision
/// check spans both.
fn sweep(schema: &str, base: &[&PredicateDecl], derived: &[&PredicateDecl]) -> Vec<ViewRefusal> {
    let mut refusals = Vec::new();

    if !is_safe_lower_ident(schema) || schema.len() > MAX_IDENT_BYTES {
        refusals.push(ViewRefusal::InvalidSchema {
            schema: schema.to_string(),
        });
    }

    let all: Vec<&PredicateDecl> = base.iter().chain(derived.iter()).copied().collect();
    for predicate in &all {
        for arg in &predicate.args {
            check_identifier(
                format!("predicate `{}` field `{}`", predicate.name, arg.name),
                &arg.name,
                &mut refusals,
            );
        }
        // View names follow the same rules as fields. The `_morpholog_`
        // prefix rule also protects `_morpholog_catalog`.
        let view = snake_case(predicate.name.as_str());
        check_identifier(
            format!("predicate `{}` (view name `{view}`)", predicate.name),
            &view,
            &mut refusals,
        );
    }

    // snake_case is many-to-one, so two distinct predicate names can render
    // to the same view.
    let mut by_view: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for predicate in &all {
        by_view
            .entry(snake_case(predicate.name.as_str()))
            .or_default()
            .push(predicate.name.as_str());
    }
    for (generated, sources) in by_view {
        if sources.len() > 1 {
            refusals.push(ViewRefusal::ViewNameCollision {
                generated,
                sources: sources.iter().map(ToString::to_string).collect(),
            });
        }
    }

    refusals
}

/// Render the complete atomic script. Assumes the sweep has passed.
fn render(
    program: &str,
    schema: &str,
    model_hash: &str,
    base: &[&PredicateDecl],
    derived: &[&PredicateDecl],
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "-- Generated by `morpholog generate views` - do not edit."
    );
    let _ = writeln!(out, "-- programme: {}", comment_text(program));
    let _ = writeln!(out, "-- model hash: {}", comment_text(model_hash));
    out.push('\n');
    out.push_str("BEGIN;\n\n");
    let _ = writeln!(out, "CREATE SCHEMA IF NOT EXISTS {};", quote_ident(schema));

    if base.is_empty() && derived.is_empty() {
        out.push_str("\n-- no predicates: this programme declares no claim shapes.\n");
    }
    for predicate in base {
        render_view(&mut out, schema, model_hash, predicate, ViewKind::Base);
    }
    for predicate in derived {
        render_view(&mut out, schema, model_hash, predicate, ViewKind::Derived);
    }

    render_catalog(&mut out, program, schema, model_hash, base, derived);
    render_seal(&mut out, schema, base, derived);

    out.push_str("\nCOMMIT;\n");
    out
}

/// Render the seal: in the same transaction, read each view's definition
/// back with `pg_get_viewdef` and store its hash. Hashing what PostgreSQL
/// stores, not the DDL above, catches a later `CREATE OR REPLACE VIEW`
/// under the same name, so `verify --views-schema` can prove the surface
/// intact. The catalogue view is sealed too.
fn render_seal(
    out: &mut String,
    schema: &str,
    base: &[&PredicateDecl],
    derived: &[&PredicateDecl],
) {
    let qualified = format!("{}.{}", quote_ident(schema), quote_ident(VIEW_DEFS_TABLE));
    let names: Vec<String> = base
        .iter()
        .chain(derived.iter())
        .map(|p| snake_case(p.name.as_str()))
        .chain(std::iter::once(CATALOG_VIEW.to_string()))
        .collect();

    out.push_str("\n-- Seal: each view's definition as PostgreSQL stores it, hashed.\n");
    let _ = writeln!(
        out,
        "CREATE TABLE IF NOT EXISTS {qualified} (view_name text PRIMARY KEY, definition_sha256 text NOT NULL);"
    );
    let _ = writeln!(out, "TRUNCATE {qualified};");
    let _ = writeln!(
        out,
        "INSERT INTO {qualified} (view_name, definition_sha256)"
    );
    let _ = writeln!(
        out,
        "SELECT v.name, encode(sha256(convert_to(pg_get_viewdef(format('%I.%I', {}, v.name)::regclass, true), 'UTF8')), 'hex')",
        quote_literal(schema)
    );
    out.push_str("FROM ( VALUES\n");
    for (idx, name) in names.iter().enumerate() {
        let cast = if idx == 0 { "::text" } else { "" };
        let comma = if idx + 1 < names.len() { "," } else { "" };
        let _ = writeln!(out, "    ({}{cast}){comma}", quote_literal(name));
    }
    out.push_str(") AS v(name);\n");
}

/// Render one predicate's view block: the read-only view, then its
/// `COMMENT ON VIEW` / `COMMENT ON COLUMN` metadata.
fn render_view(
    out: &mut String,
    schema: &str,
    model_hash: &str,
    predicate: &PredicateDecl,
    kind: ViewKind,
) {
    let name = predicate.name.as_str();
    let view = snake_case(name);
    let qualified = format!("{}.{}", quote_ident(schema), quote_ident(&view));

    let _ = writeln!(out, "\n-- View for {} predicate `{name}`.", kind.label());
    let _ = writeln!(out, "CREATE OR REPLACE VIEW {qualified} AS");

    // The CTE binds `arguments` plus the source's metadata columns. The
    // top-level `WITH` makes the view read-only.
    let (cte_name, mut selects): (&str, Vec<(String, String)>) = match kind {
        ViewKind::Base => {
            out.push_str("WITH governed_claims AS (\n");
            out.push_str("    SELECT arguments, asserted_in, asserted_at\n");
            out.push_str("    FROM morpholog.claims\n");
            let _ = writeln!(out, "    WHERE predicate_name = {}", quote_literal(name));
            out.push_str(")\n");
            (
                "governed_claims",
                vec![
                    ("asserted_in".into(), "_morpholog_asserted_in".into()),
                    ("asserted_at".into(), "_morpholog_asserted_at".into()),
                    ("arguments".into(), "_morpholog_arguments".into()),
                ],
            )
        }
        ViewKind::Derived => {
            // Filter on the model hash too: a cache refreshed for a
            // DIFFERENT model may not match these columns. The view stays
            // empty until `refresh derived` runs for this model.
            out.push_str("WITH governed_derived AS (\n");
            out.push_str("    SELECT c.arguments, r.refreshed_at, r.model_hash,\n");
            out.push_str(
                "           r.source_snapshot_transition_id, r.source_snapshot_committed_at\n",
            );
            out.push_str("    FROM morpholog_read.derived_claims c\n");
            out.push_str(
                "    JOIN morpholog_read.derived_active a ON c.refresh_id = a.refresh_id\n",
            );
            out.push_str(
                "    JOIN morpholog_read.derived_refreshes r ON r.refresh_id = a.refresh_id\n",
            );
            let _ = writeln!(out, "    WHERE c.predicate_name = {}", quote_literal(name));
            let _ = writeln!(
                out,
                "      AND r.model_hash = {}",
                quote_literal(model_hash)
            );
            out.push_str(")\n");
            (
                "governed_derived",
                vec![
                    ("refreshed_at".into(), "_morpholog_refreshed_at".into()),
                    ("model_hash".into(), "_morpholog_model_hash".into()),
                    (
                        "source_snapshot_transition_id".into(),
                        "_morpholog_source_snapshot_transition_id".into(),
                    ),
                    (
                        "source_snapshot_committed_at".into(),
                        "_morpholog_source_snapshot_committed_at".into(),
                    ),
                    ("arguments".into(), "_morpholog_arguments".into()),
                ],
            )
        }
    };
    out.push_str("SELECT\n");

    // Metadata first, then fields in declaration order, so appending a
    // field stays a compatible CREATE OR REPLACE. `_morpholog_arguments`
    // keeps the exact value, including temporal precision.
    let mut column_comments: Vec<(String, String)> = Vec::new();
    for (i, arg) in predicate.args.iter().enumerate() {
        let col = column_sql(&arg.kind, i);
        selects.push((col.expr, arg.name.clone()));
        column_comments.push((arg.name.clone(), col.kind_comment));
    }

    let width = selects
        .iter()
        .map(|(expr, _)| expr.len())
        .max()
        .unwrap_or(0);
    for (idx, (expr, alias)) in selects.iter().enumerate() {
        let comma = if idx + 1 < selects.len() { "," } else { "" };
        let _ = writeln!(
            out,
            "    {expr:<width$} AS {}{comma}",
            quote_ident(alias),
            width = width,
        );
    }
    let _ = writeln!(out, "FROM {cte_name};");

    let view_comment = match kind {
        ViewKind::Base => format!("Generated by Morpholog; predicate={name}"),
        ViewKind::Derived => format!(
            "Generated by Morpholog; derived predicate={name}; \
             populated by `morpholog refresh derived`; empty until a refresh \
             for this model hash"
        ),
    };
    let _ = writeln!(
        out,
        "COMMENT ON VIEW {qualified} IS {};",
        quote_literal(&view_comment)
    );
    for (field, comment) in column_comments {
        let _ = writeln!(
            out,
            "COMMENT ON COLUMN {qualified}.{} IS {};",
            quote_ident(&field),
            quote_literal(&comment),
        );
    }
}

/// Render the model catalogue: programme, hash, and each predicate's view
/// with its `kind` (`base` | `derived`). `VALUES`-backed, so read-only. A
/// programme with no predicates gets a typed empty catalogue.
fn render_catalog(
    out: &mut String,
    program: &str,
    schema: &str,
    model_hash: &str,
    base: &[&PredicateDecl],
    derived: &[&PredicateDecl],
) {
    let qualified = format!("{}.{}", quote_ident(schema), quote_ident(CATALOG_VIEW));
    out.push_str("\n-- Model catalogue: the programme, its model hash, and the views generated.\n");
    let _ = writeln!(out, "CREATE OR REPLACE VIEW {qualified} AS");

    // Base first, then derived, matching the view blocks above.
    let rows: Vec<(&str, ViewKind)> = base
        .iter()
        .map(|p| (p.name.as_str(), ViewKind::Base))
        .chain(derived.iter().map(|p| (p.name.as_str(), ViewKind::Derived)))
        .collect();

    if rows.is_empty() {
        out.push_str("SELECT\n");
        out.push_str("    NULL::text AS programme_name,\n");
        out.push_str("    NULL::text AS model_hash,\n");
        out.push_str("    NULL::text AS predicate_name,\n");
        out.push_str("    NULL::text AS view_name,\n");
        out.push_str("    NULL::text AS kind\n");
        out.push_str("WHERE false;\n");
    } else {
        out.push_str("SELECT * FROM ( VALUES\n");
        for (idx, (name, kind)) in rows.iter().enumerate() {
            let view = snake_case(name);
            // Cast the first row's columns so the VALUES list is typed
            // text; later rows infer from it.
            let row = if idx == 0 {
                format!(
                    "    ({}::text, {}::text, {}::text, {}::text, {}::text)",
                    quote_literal(program),
                    quote_literal(model_hash),
                    quote_literal(name),
                    quote_literal(&view),
                    quote_literal(kind.label()),
                )
            } else {
                format!(
                    "    ({}, {}, {}, {}, {})",
                    quote_literal(program),
                    quote_literal(model_hash),
                    quote_literal(name),
                    quote_literal(&view),
                    quote_literal(kind.label()),
                )
            };
            let comma = if idx + 1 < rows.len() { "," } else { "" };
            let _ = writeln!(out, "{row}{comma}");
        }
        out.push_str(
            ") AS generated(programme_name, model_hash, predicate_name, view_name, kind);\n",
        );
    }
    let _ = writeln!(
        out,
        "COMMENT ON VIEW {qualified} IS {};",
        quote_literal("Generated by Morpholog; the model catalogue for this view surface")
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
