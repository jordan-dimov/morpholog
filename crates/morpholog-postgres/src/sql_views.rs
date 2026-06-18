//! `morpholog generate views` - render a programme's BASE predicates as
//! a typed, read-only SQL view surface over `morpholog.claims`.
//!
//! This is the read-side complement of `morpholog schema`: where `schema`
//! describes the write contract, this projects the admitted state into
//! plain relational SQL any BI or analytics tool can read. The renderer
//! is **pure** (no sqlx, no async, no DB): same inputs produce
//! byte-identical bytes, so the embedder's drift discipline is
//! regenerate-and-diff, exactly as for the generated Python client.
//!
//! It lives in `morpholog-postgres`, not the kernel, because SQL over
//! `morpholog.claims` - the JSONB shape, the PG types, the extractor
//! operators - is PostgreSQL-substrate knowledge. The crate already owns
//! the claims<->JSONB wire mapping (`decode_claim_rows`), so the
//! kind->PG-type match co-locates with the shape it mirrors. The single
//! source of truth for the JSONB shape is `EvalValue` in
//! `morpholog-core/src/state.rs` (`#[serde(tag="type", content="value")]`);
//! the extractors here mirror it position-for-position.
//!
//! Four properties make the output a credible read *contract* rather than
//! a convenience dump:
//!   1. **Non-updatable by construction** - each view wraps its source in
//!      a top-level `WITH`, which disqualifies it from PostgreSQL's
//!      automatic updatability, so `INSERT`/`UPDATE`/`DELETE` through it
//!      fail rather than reaching `morpholog.claims`.
//!   2. **Atomic** - the whole script is wrapped `BEGIN; ... COMMIT;`, so
//!      a database-time failure leaves no half-updated read surface.
//!   3. **Metadata-first columns** plus the raw `_morpholog_arguments`
//!      column, so appending a declared field stays a compatible
//!      `CREATE OR REPLACE VIEW`, and the exact governed value is always
//!      available behind the typed projection.
//!   4. **Hash-pinned** - a `_morpholog_catalog` view carries the model
//!      hash and the intended view inventory, the same pin the generated
//!      Python client records.
//!
//! Refusal is whole-run (mirrors `generate.rs::sweep`): every
//! un-emittable identifier across the base vocabulary is collected before
//! anything is rendered, and any finding fails the run with the full work
//! list and nothing written. Derived-claim heads are skipped (they are
//! computed on demand, never materialized - see the module docs and
//! `docs/roadmap.md` for the deferred kernel->SQL spike).

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

use morpholog_core::{PredicateArgKind, PredicateDecl, ValidatedProgram};

/// The rendered script plus the count of base-predicate views it emits
/// (excluding the catalogue). `view_count` lets the CLI print its summary
/// without independently reconstructing the base/derived distinction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedViews {
    pub sql: String,
    pub view_count: usize,
}

/// One reason a programme cannot be rendered as a view surface. Collected
/// across the whole base vocabulary so the author sees one complete work
/// list. The CLI maps each variant to a single `error:` line via
/// [`std::fmt::Display`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewRefusal {
    /// A preserved field name is not a safe unquoted SQL identifier
    /// (`[a-z_][a-z0-9_]*`). PostgreSQL folds unquoted identifiers to
    /// lowercase, so an uppercase name would silently diverge from the
    /// declaration; refusing beats mangling.
    InvalidIdentifier { owner: String, name: String },
    /// An identifier exceeds PostgreSQL's 63-byte limit, beyond which it
    /// is silently truncated - which can collide invisibly.
    IdentifierTooLong { owner: String, name: String },
    /// A field name is a SQL reserved word; refusing means consumers
    /// never have to quote it in their own queries.
    ReservedKeyword { owner: String, name: String },
    /// A business field name starts with the generator-owned
    /// `_morpholog_` prefix, which would shadow a metadata column.
    ReservedPrefix { owner: String, name: String },
    /// Two base predicates whose names render to the same snake_case view
    /// name. Both sources are named so the author knows which to rename.
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
            ViewRefusal::ReservedKeyword { owner, name } => {
                write!(f, "{owner}: `{name}` is a SQL reserved word; rename it")
            }
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

/// The generator-owned column prefix. Reserved as a whole namespace
/// (rather than the individual provenance names) so a legitimate business
/// field called `predicate_name` or `asserted_at` is lawful.
const MORPHOLOG_PREFIX: &str = "_morpholog_";

/// PostgreSQL's identifier length limit (`NAMEDATALEN - 1`). Names beyond
/// this are silently truncated.
const MAX_IDENT_BYTES: usize = 63;

/// The catalogue view's name. Carries the model hash and the intended
/// view inventory for drift-checking.
const CATALOG_VIEW: &str = "_morpholog_catalog";

/// SQL reserved words (the PG17/PG18 fully-reserved union). A field or
/// view name matching one is refused so consumers need not quote it. Kept
/// as a sorted slice; lookups lowercase the candidate first.
const SQL_KEYWORDS: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "current_catalog",
    "current_date",
    "current_role",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "grant",
    "group",
    "having",
    "in",
    "initially",
    "intersect",
    "into",
    "lateral",
    "leading",
    "limit",
    "localtime",
    "localtimestamp",
    "not",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "placing",
    "primary",
    "references",
    "returning",
    "select",
    "session_user",
    "some",
    "symmetric",
    "system_user",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "when",
    "where",
    "window",
    "with",
];

/// The SQL `SELECT` expression and the kind note for one declared
/// argument position. The expression is over the CTE-bound `arguments`
/// JSONB array; `kind_comment` becomes a persistent `COMMENT ON COLUMN`.
struct ColumnSql {
    expr: String,
    kind_comment: String,
}

/// The PG type + extractor for one argument kind at positional index `i`.
///
/// Exhaustive over [`PredicateArgKind`] with **no `_` arm**: a new kind
/// must fail compilation here, forcing a deliberate decision rather than
/// a silently-wrong projection. The shapes mirror `EvalValue` in
/// `morpholog-core/src/state.rs` position-for-position. Every kind is
/// representable - `Collection` and `Any` fall back to faithful `jsonb`
/// rather than being refused (they diverge from the python-client floor,
/// which refuses them, because a read projection can carry them).
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
            // jiff serialises a negative span with a LEADING sign
            // (`-PT6H`), which PostgreSQL's interval parser rejects (it
            // wants the sign inside, `PT-6H`). Strip the leading `-` and
            // negate, so one negative-duration claim does not break the
            // whole view at read time. Sub-microsecond components still
            // truncate (the documented precision boundary).
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
        PredicateArgKind::Any => col(
            format!("arguments -> {i}"),
            "Morpholog kind Any (the whole tagged {type,value} object)".to_string(),
        ),
    }
}

/// `TradeSettled` -> `trade_settled`, acronym-aware: `PPAContract` ->
/// `ppa_contract`, `TradeID` -> `trade_id`, `HTTP2Request` ->
/// `http2_request`. Inserts `_` before an uppercase letter that follows a
/// lowercase or digit, and before an uppercase letter that begins a word
/// after an acronym (uppercase preceded by uppercase, followed by
/// lowercase); lowercases everything.
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

/// Push every identifier refusal for `name` under `owner`. Applied
/// identically to a preserved field name and to a generated view name -
/// so a predicate named `Order` is refused exactly as a field named
/// `order` would be, and consumers never have to quote a generated name.
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
            owner: owner.clone(),
            name: name.to_string(),
        });
    }
    if SQL_KEYWORDS.contains(&name.to_ascii_lowercase().as_str()) {
        refusals.push(ViewRefusal::ReservedKeyword {
            owner,
            name: name.to_string(),
        });
    }
}

/// Double-quote a SQL identifier, doubling any embedded quote. Every
/// identifier in the generated script is quoted, even safe ones, so the
/// renderer never depends on PostgreSQL's case-folding.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Single-quote a SQL string literal, doubling any embedded apostrophe.
/// Every literal in the script - programme name, predicate name, hash,
/// comment body - goes through here.
fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Make a value safe to interpolate into a `--` line comment. A `--`
/// comment runs to end of line, so a newline in the value would break
/// out of it - and since `render_views` is public and takes an arbitrary
/// `model_hash` (and a `Program.name` the kernel does not SQL-validate),
/// a newline could inject text after the comment. Escape CR and LF.
fn comment_text(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// Render the atomic `CREATE VIEW` script for `program`'s BASE predicates
/// over `morpholog.claims`, into `schema`. Derived-claim heads are
/// skipped. Pure: identical inputs produce byte-identical output. On any
/// refusal returns every finding and renders nothing.
pub fn render_views(
    program: ValidatedProgram<'_>,
    schema: &str,
    model_hash: &str,
) -> Result<RenderedViews, Vec<ViewRefusal>> {
    let program = program.as_program();

    // Derived-claim heads are declared as predicates but computed on
    // demand, never materialized; they get no view.
    let derived: HashSet<&str> = program
        .derived_claims
        .iter()
        .map(|d| d.predicate.as_str())
        .collect();
    let base: Vec<&PredicateDecl> = program
        .predicates
        .iter()
        .filter(|p| !derived.contains(p.name.as_str()))
        .collect();

    let refusals = sweep(schema, &base);
    if !refusals.is_empty() {
        return Err(refusals);
    }

    let sql = render(program.name.as_str(), schema, model_hash, &base);
    Ok(RenderedViews {
        sql,
        view_count: base.len(),
    })
}

/// Collect every reason this programme's base vocabulary cannot be
/// rendered, across the whole surface, so the author sees one complete
/// work list (the same whole-run discipline as `generate.rs::sweep`).
fn sweep(schema: &str, base: &[&PredicateDecl]) -> Vec<ViewRefusal> {
    let mut refusals = Vec::new();

    if !is_safe_lower_ident(schema) || schema.len() > MAX_IDENT_BYTES {
        refusals.push(ViewRefusal::InvalidSchema {
            schema: schema.to_string(),
        });
    }

    for predicate in base {
        for arg in &predicate.args {
            check_identifier(
                format!("predicate `{}` field `{}`", predicate.name, arg.name),
                &arg.name,
                &mut refusals,
            );
        }
        // The generated view name gets the SAME rules as a field: a
        // predicate named `Order`, `User`, or `Select` would otherwise
        // emit a reserved-word view, forcing consumers to quote it -
        // against the unquoted-read contract. Length and the reserved
        // `_morpholog_` namespace (which protects `_morpholog_catalog`)
        // are covered by the same helper.
        let view = snake_case(predicate.name.as_str());
        check_identifier(
            format!("predicate `{}` (view name `{view}`)", predicate.name),
            &view,
            &mut refusals,
        );
    }

    // snake_case is many-to-one, and Morpholog's duplicate check is on
    // exact names - so two lawful predicate names can collide at the
    // generated view. Refuse with both sources named.
    let mut by_view: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for predicate in base {
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

/// Render the complete atomic script. Pure string-building; assumes the
/// sweep has already passed (so every identifier is safe to quote).
fn render(program: &str, schema: &str, model_hash: &str, base: &[&PredicateDecl]) -> String {
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

    if base.is_empty() {
        out.push_str(
            "\n-- no base predicates: this programme declares no materialized claim shapes.\n",
        );
    }
    for predicate in base {
        render_view(&mut out, schema, predicate);
    }

    render_catalog(&mut out, program, schema, model_hash, base);

    out.push_str("\nCOMMIT;\n");
    out
}

/// Render one predicate's view block: the non-updatable CTE view, then
/// the persistent `COMMENT ON VIEW` / `COMMENT ON COLUMN` metadata.
fn render_view(out: &mut String, schema: &str, predicate: &PredicateDecl) {
    let name = predicate.name.as_str();
    let view = snake_case(name);
    let qualified = format!("{}.{}", quote_ident(schema), quote_ident(&view));

    let _ = writeln!(out, "\n-- View for predicate `{name}`.");
    let _ = writeln!(out, "CREATE OR REPLACE VIEW {qualified} AS");
    out.push_str("WITH governed_claims AS (\n");
    out.push_str("    SELECT arguments, asserted_in, asserted_at\n");
    out.push_str("    FROM morpholog.claims\n");
    let _ = writeln!(out, "    WHERE predicate_name = {}", quote_literal(name));
    out.push_str(")\n");
    out.push_str("SELECT\n");

    // Metadata-first, then business fields in declaration order. The raw
    // _morpholog_arguments preserves the exact governed value behind the
    // typed projection (and the precision floor for temporal kinds).
    let mut selects: Vec<(String, String)> = vec![
        (
            "asserted_in".to_string(),
            "_morpholog_asserted_in".to_string(),
        ),
        (
            "asserted_at".to_string(),
            "_morpholog_asserted_at".to_string(),
        ),
        ("arguments".to_string(), "_morpholog_arguments".to_string()),
    ];
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
    out.push_str("FROM governed_claims;\n");

    let _ = writeln!(
        out,
        "COMMENT ON VIEW {qualified} IS {};",
        quote_literal(&format!("Generated by Morpholog; predicate={name}"))
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

/// Render the model catalogue: programme, hash, and the predicate->view
/// inventory. `VALUES`-backed so it is inherently non-updatable. A
/// zero-base programme gets a typed-empty catalogue.
fn render_catalog(
    out: &mut String,
    program: &str,
    schema: &str,
    model_hash: &str,
    base: &[&PredicateDecl],
) {
    let qualified = format!("{}.{}", quote_ident(schema), quote_ident(CATALOG_VIEW));
    out.push_str("\n-- Model catalogue: the programme, its model hash, and the views generated.\n");
    let _ = writeln!(out, "CREATE OR REPLACE VIEW {qualified} AS");

    if base.is_empty() {
        out.push_str("SELECT\n");
        out.push_str("    NULL::text AS programme_name,\n");
        out.push_str("    NULL::text AS model_hash,\n");
        out.push_str("    NULL::text AS predicate_name,\n");
        out.push_str("    NULL::text AS view_name\n");
        out.push_str("WHERE false;\n");
    } else {
        out.push_str("SELECT * FROM ( VALUES\n");
        for (idx, predicate) in base.iter().enumerate() {
            let name = predicate.name.as_str();
            let view = snake_case(name);
            // Cast the first row's columns so the VALUES list is typed
            // text; later rows infer from it.
            let row = if idx == 0 {
                format!(
                    "    ({}::text, {}::text, {}::text, {}::text)",
                    quote_literal(program),
                    quote_literal(model_hash),
                    quote_literal(name),
                    quote_literal(&view),
                )
            } else {
                format!(
                    "    ({}, {}, {}, {})",
                    quote_literal(program),
                    quote_literal(model_hash),
                    quote_literal(name),
                    quote_literal(&view),
                )
            };
            let comma = if idx + 1 < base.len() { "," } else { "" };
            let _ = writeln!(out, "{row}{comma}");
        }
        out.push_str(") AS generated(programme_name, model_hash, predicate_name, view_name);\n");
    }
    let _ = writeln!(
        out,
        "COMMENT ON VIEW {qualified} IS {};",
        quote_literal("Generated by Morpholog; the model catalogue for this view surface")
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use morpholog_core::{ArgDecl, PredicateDecl, Program};
    use morpholog_surface::parse_program;

    fn decl(name: &str, args: &[(&str, PredicateArgKind)]) -> PredicateDecl {
        PredicateDecl {
            name: name.into(),
            args: args
                .iter()
                .map(|(n, k)| ArgDecl {
                    name: (*n).to_string(),
                    kind: k.clone(),
                })
                .collect(),
            disciplines: Vec::new(),
        }
    }

    fn render_ok(src: &str) -> RenderedViews {
        let program = parse_program(src).expect("fixture parses");
        // Leak so the ValidatedProgram borrow lives for the call; tests
        // are short-lived processes.
        let program: &'static Program = Box::leak(Box::new(program));
        let validated = program.validated().expect("fixture validates");
        render_views(validated, "morpholog_views", "sha256:testhash").expect("renders")
    }

    fn refusals(src: &str, schema: &str) -> Vec<ViewRefusal> {
        let program = parse_program(src).expect("fixture parses");
        let program: &'static Program = Box::leak(Box::new(program));
        let validated = program.validated().expect("fixture validates");
        render_views(validated, schema, "sha256:testhash").expect_err("refuses")
    }

    // ---- snake_case (pinned, acronym-aware) ----

    #[test]
    fn snake_case_is_acronym_aware() {
        assert_eq!(snake_case("TradeSettled"), "trade_settled");
        assert_eq!(snake_case("PPAContract"), "ppa_contract");
        assert_eq!(snake_case("TradeID"), "trade_id");
        assert_eq!(snake_case("HTTP2Request"), "http2_request");
        assert_eq!(snake_case("VoyageFixture"), "voyage_fixture");
    }

    // ---- column_sql: one assertion per kind ----

    #[test]
    fn column_sql_subject_has_no_cast() {
        let c = column_sql(&PredicateArgKind::Subject, 0);
        assert_eq!(c.expr, "arguments -> 0 ->> 'value'");
    }

    #[test]
    fn column_sql_typed_scalars_cast() {
        assert_eq!(
            column_sql(&PredicateArgKind::Decimal, 1).expr,
            "(arguments -> 1 ->> 'value')::numeric"
        );
        assert_eq!(
            column_sql(&PredicateArgKind::Date, 2).expr,
            "(arguments -> 2 ->> 'value')::date"
        );
        assert_eq!(
            column_sql(&PredicateArgKind::Timestamp, 3).expr,
            "(arguments -> 3 ->> 'value')::timestamptz"
        );
        assert_eq!(
            column_sql(&PredicateArgKind::Duration, 4).expr,
            "CASE WHEN (arguments -> 4 ->> 'value') LIKE '-%' \
             THEN -((substring(arguments -> 4 ->> 'value' FROM 2))::interval) \
             ELSE (arguments -> 4 ->> 'value')::interval END"
        );
        assert_eq!(
            column_sql(&PredicateArgKind::Bool, 5).expr,
            "(arguments -> 5 ->> 'value')::boolean"
        );
    }

    #[test]
    fn column_sql_quantity_reaches_amount_and_notes_unit() {
        let c = column_sql(&PredicateArgKind::Quantity("USD".into()), 2);
        assert_eq!(c.expr, "(arguments -> 2 -> 'value' ->> 'amount')::numeric");
        assert!(
            c.kind_comment.contains("amount in USD"),
            "{}",
            c.kind_comment
        );
    }

    #[test]
    fn column_sql_collection_is_value_array_jsonb() {
        assert_eq!(
            column_sql(&PredicateArgKind::Collection, 0).expr,
            "arguments -> 0 -> 'value'"
        );
    }

    #[test]
    fn column_sql_any_is_the_whole_tagged_object() {
        assert_eq!(column_sql(&PredicateArgKind::Any, 7).expr, "arguments -> 7");
    }

    // ---- render_views: shape, determinism, exclusion ----

    const MIXED: &str = "program mixed\n\
        predicate VoyageFixture(voyage: Subject, vessel: Subject, allowed: Duration)\n\
        transformation t(voyage, vessel, allowed):\n    \
            admit VoyageFixture(voyage, vessel, allowed)\n";

    #[test]
    fn render_emits_atomic_nonupdatable_metadata_first_view() {
        let r = render_ok(MIXED);
        let sql = &r.sql;
        assert_eq!(r.view_count, 1);
        // atomic
        assert!(sql.starts_with("-- Generated by"));
        assert!(sql.contains("BEGIN;"));
        assert!(sql.trim_end().ends_with("COMMIT;"));
        // model hash in header
        assert!(sql.contains("-- model hash: sha256:testhash"));
        // non-updatable CTE
        assert!(sql.contains("WITH governed_claims AS ("));
        assert!(sql.contains("WHERE predicate_name = 'VoyageFixture'"));
        // metadata-first, then business fields in declaration order
        let meta_in = sql.find("_morpholog_asserted_in").unwrap();
        let meta_args = sql.find("_morpholog_arguments").unwrap();
        let voyage = sql.find("AS \"voyage\"").unwrap();
        let allowed = sql.find("AS \"allowed\"").unwrap();
        assert!(meta_in < meta_args && meta_args < voyage && voyage < allowed);
        // typed projection (Duration uses the sign-correcting CASE) + comments
        assert!(sql.contains("CASE WHEN (arguments -> 2 ->> 'value') LIKE '-%'"));
        assert!(sql.contains("END AS \"allowed\""));
        assert!(sql.contains("COMMENT ON VIEW \"morpholog_views\".\"voyage_fixture\" IS"));
        assert!(
            sql.contains("COMMENT ON COLUMN \"morpholog_views\".\"voyage_fixture\".\"allowed\"")
        );
    }

    #[test]
    fn render_is_byte_deterministic() {
        assert_eq!(render_ok(MIXED).sql, render_ok(MIXED).sql);
    }

    #[test]
    fn catalog_carries_hash_and_inventory() {
        let r = render_ok(MIXED);
        assert!(
            r.sql
                .contains("CREATE OR REPLACE VIEW \"morpholog_views\".\"_morpholog_catalog\"")
        );
        assert!(r.sql.contains("'mixed'::text"));
        assert!(r.sql.contains("'sha256:testhash'::text"));
        assert!(r.sql.contains("'VoyageFixture'::text"));
        assert!(r.sql.contains("'voyage_fixture'::text"));
    }

    #[test]
    fn quantity_and_any_and_collection_render() {
        let src = "program kinds\n\
            predicate K(id: Subject, daily: Decimal[USD], tags: Collection, blob: Any)\n\
            transformation t(id, daily, tags, blob):\n    \
                admit K(id, daily, tags, blob)\n";
        let sql = render_ok(src).sql;
        assert!(sql.contains("(arguments -> 1 -> 'value' ->> 'amount')::numeric AS \"daily\""));
        assert!(sql.contains("arguments -> 2 -> 'value'"));
        assert!(sql.contains("arguments -> 3"));
        assert!(sql.contains("amount in USD"));
    }

    #[test]
    fn derived_heads_are_excluded() {
        // trade_lifecycle declares TermsTimeline as a predicate AND a
        // derived block; it must not get a view, while base predicates do.
        let program: &'static Program =
            Box::leak(Box::new(morpholog_examples::trade_lifecycle::program()));
        let validated = program.validated().expect("validates");
        let r = render_views(validated, "morpholog_views", "sha256:x").expect("renders");
        assert!(
            r.sql.contains("\"trade_settled\""),
            "base predicate present"
        );
        assert!(
            !r.sql.contains("\"terms_timeline\""),
            "derived head must be excluded"
        );
    }

    #[test]
    fn zero_base_predicates_is_lawful_empty() {
        // A programme with no materialized claim shapes is lawful-empty,
        // not a refusal: a typed-empty catalogue, no views.
        let sql = render("empty", "morpholog_views", "sha256:testhash", &[]);
        assert!(sql.contains("-- no base predicates"));
        assert!(sql.contains("NULL::text AS programme_name"));
        assert!(sql.contains("WHERE false;"));
        assert!(sql.contains("BEGIN;") && sql.trim_end().ends_with("COMMIT;"));
    }

    // ---- refusals ----

    #[test]
    fn reserved_keyword_field_is_refused() {
        let src = "program kw\n\
            predicate P(id: Subject, select: Decimal)\n\
            transformation t(id, select):\n    admit P(id, select)\n";
        let found = refusals(src, "morpholog_views");
        assert!(found.iter().any(|r| matches!(
            r,
            ViewRefusal::ReservedKeyword { name, .. } if name == "select"
        )));
    }

    #[test]
    fn morpholog_prefixed_field_is_refused() {
        // The surface grammar may not even permit a leading-underscore
        // field, but the prefix reservation is a defensive guarantee on
        // the rendered surface - exercise the sweep directly.
        let p = decl(
            "P",
            &[
                ("id", PredicateArgKind::Subject),
                ("_morpholog_x", PredicateArgKind::Decimal),
            ],
        );
        let found = sweep("morpholog_views", &[&p]);
        assert!(found.iter().any(|r| matches!(
            r,
            ViewRefusal::ReservedPrefix { name, .. } if name == "_morpholog_x"
        )));
    }

    #[test]
    fn over_long_identifier_is_refused() {
        let long = "a".repeat(64);
        let p = decl("P", &[(long.as_str(), PredicateArgKind::Decimal)]);
        let found = sweep("morpholog_views", &[&p]);
        assert!(
            found
                .iter()
                .any(|r| matches!(r, ViewRefusal::IdentifierTooLong { .. }))
        );
    }

    #[test]
    fn colliding_view_names_are_refused_naming_both() {
        // `TradeID` and `Trade_id` both snake to `trade_id`.
        let src = "program coll\n\
            predicate TradeID(id: Subject)\n\
            predicate Trade_id(id: Subject)\n\
            transformation t(id):\n    admit TradeID(id)\n";
        let found = refusals(src, "morpholog_views");
        assert!(found.iter().any(|r| matches!(
            r,
            ViewRefusal::ViewNameCollision { generated, .. } if generated == "trade_id"
        )));
    }

    #[test]
    fn invalid_schema_is_refused() {
        let found = refusals(MIXED, "Bad Schema!");
        assert!(
            found
                .iter()
                .any(|r| matches!(r, ViewRefusal::InvalidSchema { .. }))
        );
    }

    #[test]
    fn refusal_renders_nothing() {
        // A refusal returns Err - there is no String to leak.
        let found = refusals(MIXED, "0bad");
        assert!(!found.is_empty());
    }

    #[test]
    fn keyword_view_names_are_refused() {
        // A predicate named `Order` or `User` snakes to a reserved word;
        // refuse it so consumers never have to quote the generated view.
        let src = "program kwviews\n\
            predicate Order(id: Subject)\n\
            predicate User(id: Subject)\n\
            transformation t(id):\n    admit Order(id)\n";
        let found = refusals(src, "morpholog_views");
        assert!(found.iter().any(|r| matches!(
            r,
            ViewRefusal::ReservedKeyword { name, .. } if name == "order"
        )));
        assert!(found.iter().any(|r| matches!(
            r,
            ViewRefusal::ReservedKeyword { name, .. } if name == "user"
        )));
    }

    #[test]
    fn morpholog_prefixed_view_name_is_refused() {
        // A predicate whose snake_case enters the `_morpholog_` namespace
        // would collide with `_morpholog_catalog`; refuse it. Exercised
        // via the sweep directly - the surface grammar may not permit such
        // a predicate name.
        let p = decl("_morphologThing", &[("id", PredicateArgKind::Subject)]);
        let found = sweep("morpholog_views", &[&p]);
        assert!(found.iter().any(|r| matches!(
            r,
            ViewRefusal::ReservedPrefix { name, .. } if name == "_morpholog_thing"
        )));
    }

    #[test]
    fn header_values_cannot_break_out_of_the_comment() {
        // render() is reachable with an arbitrary programme name / hash
        // through the public API; a newline must not escape the `--`
        // header comment and inject SQL after it.
        let sql = render("evil\nDROP TABLE x;", "morpholog_views", "sha256:a\nb", &[]);
        assert!(
            !sql.contains("\nDROP TABLE x;"),
            "a newline escaped the header comment: {sql}"
        );
        assert!(sql.contains("evil\\nDROP TABLE x;"));
        assert!(sql.contains("sha256:a\\nb"));
    }
}
