//! Human-readable pretty-printer for [`Program`]s and their components.
//!
//! Renders IR as readable indented text for test panics, CLI output,
//! and kernel diagnostics.
//!
//! Two contracts, told apart by signature. Renderers that take a
//! [`Program`] (`format_program`, `canonical_hash`, the `*_source`
//! helpers) emit source that round-trips through `parse_program`. The
//! decl-free inline helpers emit display text that may not reparse.
//!
//! Statement bodies expand vertically, one concept per line. Output
//! ends with a newline, and so does each section helper's block.
//!
//! The matches are exhaustive, so a new IR variant cannot compile
//! without a rendering.

use crate::{
    ArithOp, Claim, CompareOp, Definition, DerivedClaim, Discipline, Intent, Invariant,
    InvariantOrigin, OrderedDomain, PredicateDecl, Program, Prop, Stmt, Term, Transformation,
    Value, ValueExpr, Var,
};

/// The surface token for an ordered comparison, used by the formatter
/// and the checker's diagnostics. The parser holds the inverse mapping;
/// the round-trip test keeps them in step.
pub(crate) fn compare_token(op: CompareOp, domain: OrderedDomain) -> &'static str {
    match (domain, op) {
        (OrderedDomain::Decimal, CompareOp::Le) => "<=",
        (OrderedDomain::Decimal, CompareOp::Lt) => "<",
        (OrderedDomain::Decimal, CompareOp::Ge) => ">=",
        (OrderedDomain::Decimal, CompareOp::Gt) => ">",
        (OrderedDomain::Date, CompareOp::Le) => "on_or_before",
        (OrderedDomain::Date, CompareOp::Lt) => "before",
        (OrderedDomain::Date, CompareOp::Ge) => "on_or_after",
        (OrderedDomain::Date, CompareOp::Gt) => "after",
        // Instants: "at" suits a point in time; strictly_* makes the
        // boundary explicit.
        (OrderedDomain::Timestamp, CompareOp::Le) => "at_or_before",
        (OrderedDomain::Timestamp, CompareOp::Lt) => "strictly_before",
        (OrderedDomain::Timestamp, CompareOp::Ge) => "at_or_after",
        (OrderedDomain::Timestamp, CompareOp::Gt) => "strictly_after",
        // Spans compare as lengths: `counted no_longer_than allowed`.
        (OrderedDomain::Duration, CompareOp::Le) => "no_longer_than",
        (OrderedDomain::Duration, CompareOp::Lt) => "shorter_than",
        (OrderedDomain::Duration, CompareOp::Ge) => "no_shorter_than",
        (OrderedDomain::Duration, CompareOp::Gt) => "longer_than",
    }
}

/// The surface token for a binary arithmetic operator. Infix operators
/// (`is_infix`) print between their operands, the rest as `token(l, r)`.
/// The parser holds the inverse mapping; the round-trip test keeps them
/// in step.
pub(crate) fn arith_token(op: ArithOp) -> &'static str {
    match op {
        ArithOp::Add => "+",
        ArithOp::Sub => "-",
        ArithOp::Mul => "*",
        ArithOp::Div => "/",
        ArithOp::Mod => "%",
    }
}

/// The content hash of a programme: `sha256:<hex>` over a stable
/// positional rendering of the parsed IR, not [`format_program`], whose
/// output may evolve. It identifies the rules, not the file: formatting,
/// comments, and equivalent sugar (named vs positional patterns) do not
/// change it. The prefix names the algorithm.
pub fn canonical_hash(p: &Program) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    // Positional, so a nicer spelling in the formatter never moves the
    // hash. The declaration table is still needed for a `value` lookup
    // whose hole is not its first wildcard, which has no positional form.
    let naming = claim_naming(p);
    let digest = Sha256::digest(
        render_program(
            p,
            FormatContext {
                predicates: Some(&naming),
                named_canonical: false,
            },
        )
        .as_bytes(),
    );
    let mut out = String::with_capacity(7 + digest.len() * 2);
    out.push_str("sha256:");
    for b in digest {
        // Writing into a String cannot fail.
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// The declaration table the named forms resolve field names from,
/// when the caller holds a whole programme.
type Naming<'a> = Option<&'a std::collections::HashMap<&'a str, &'a PredicateDecl>>;

/// What every formatting call carries. `predicates` supplies field
/// names; it is `None` for diagnostic display text (rejection reasons,
/// witnesses). `named_canonical` turns on the human-facing form, where
/// wildcard runs print named; off, the rendering is the stable
/// positional one the hash uses. In either mode, a `value` lookup whose
/// hole is not its first wildcard prints named when it can, because the
/// positional text would reparse differently.
#[derive(Clone, Copy)]
pub(crate) struct FormatContext<'a> {
    predicates: Naming<'a>,
    named_canonical: bool,
}

impl FormatContext<'_> {
    /// Decl-free positional rendering: diagnostics and inline helpers.
    pub(crate) const DIAGNOSTIC: FormatContext<'static> = FormatContext {
        predicates: None,
        named_canonical: false,
    };
}

/// The field-name table for the named form. Duplicate predicate names
/// and repeated field names are left out: such programmes do not
/// validate, and named text for them would resolve wrongly.
fn claim_naming(p: &Program) -> std::collections::HashMap<&str, &PredicateDecl> {
    let mut counts = std::collections::HashMap::new();
    for d in &p.predicates {
        *counts.entry(d.name.as_str()).or_insert(0usize) += 1;
    }
    p.predicates
        .iter()
        .filter(|d| counts[d.name.as_str()] == 1)
        .filter(|d| {
            let mut seen = std::collections::HashSet::new();
            d.args.iter().all(|a| seen.insert(a.name.as_str()))
        })
        .map(|d| (d.name.as_str(), d))
        .collect()
}

/// The longest run of consecutive wildcards. Two or more in a row
/// trigger the named form.
fn max_wildcard_run(args: &[Term]) -> usize {
    let mut best = 0;
    let mut run = 0;
    for t in args {
        if matches!(t, Term::Wildcard) {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}

/// Top-level entry. Returns a multi-line string terminated by a
/// final `\n`, so callers can write directly to a stream or append
/// to an existing buffer.
pub fn format_program(p: &Program) -> String {
    let naming = claim_naming(p);
    render_program(
        p,
        FormatContext {
            predicates: Some(&naming),
            named_canonical: true,
        },
    )
}

fn render_program(p: &Program, ctx: FormatContext) -> String {
    let mut out = String::new();
    out.push_str(&format!("program {}\n", p.name));

    // Predicates first, so the reader knows the vocabulary before any
    // claim uses it.
    if !p.predicates.is_empty() {
        out.push('\n');
        for decl in &p.predicates {
            out.push_str(&format_predicate_decl(decl));
        }
    }

    // Intents get their own section after predicates.
    if !p.intents.is_empty() {
        out.push('\n');
        for decl in &p.intents {
            out.push_str(&format_intent_decl(decl));
        }
    }

    for def in &p.definitions {
        // Omit by origin, not name: printing a generated selector would
        // make it authored on reparse, and an authored definition of the
        // same name must still print.
        if def.origin == crate::ir::DefinitionOrigin::Discipline {
            continue;
        }
        out.push('\n');
        out.push_str(&format_definition(def, ctx));
    }

    for inv in &p.invariants {
        // Discipline invariants come back from the declaration clauses
        // on reparse; printing them would duplicate them.
        if inv.origin == InvariantOrigin::Discipline {
            continue;
        }
        out.push('\n');
        out.push_str(&format_invariant(inv, ctx));
    }

    for t in &p.transformations {
        out.push('\n');
        out.push_str(&format_transformation(t, ctx));
    }

    for d in &p.derived_claims {
        out.push('\n');
        out.push_str(&format_derived_claim(d, ctx));
    }

    out
}

/// Render a single [`PredicateDecl`] as one line:
/// `predicate Name(arg1: Kind, arg2: Kind)`.
pub(crate) fn format_predicate_decl(decl: &PredicateDecl) -> String {
    let args: Vec<String> = decl
        .args
        .iter()
        .map(|a| format!("{}: {}", a.name, a.kind))
        .collect();
    let mut out = format!("predicate {}({})\n", decl.name, args.join(", "));
    for discipline in &decl.disciplines {
        out.push_str(&indent(1));
        out.push_str(&match discipline {
            Discipline::UniqueBy { fields } => format!("unique by ({})", fields.join(", ")),
            Discipline::AppendOnly => "append only".to_string(),
            Discipline::EffectiveBy { keys, on, partial } => {
                let gap = if *partial { " partial" } else { "" };
                format!("effective by ({}) on ({on}){gap}", keys.join(", "))
            }
            Discipline::CurrentPointerBy { fields } => {
                format!("current pointer by ({})", fields.join(", "))
            }
            Discipline::SupersededVia { lineage } => format!("superseded via {lineage}"),
        });
        out.push('\n');
    }
    out
}

/// Render a single [`crate::IntentDecl`] as one line:
/// `intent Name(arg1: Kind, arg2: Kind)`.
pub(crate) fn format_intent_decl(decl: &crate::IntentDecl) -> String {
    let args: Vec<String> = decl
        .args
        .iter()
        .map(|a| format!("{}: {}", a.name, a.kind))
        .collect();
    format!("intent {}({})\n", decl.name, args.join(", "))
}

/// Render a [`Definition`] in the invariant block shape:
/// `define name(params):` with the body indented.
pub(crate) fn format_definition(def: &Definition, ctx: FormatContext) -> String {
    let params: Vec<String> = def.parameters.iter().map(ToString::to_string).collect();
    let mut out = String::new();
    out.push_str(&format!("define {}({}):\n", def.name, params.join(", ")));
    out.push_str(&indent(1));
    out.push_str(&fmt_prop(&def.body, ctx));
    out.push('\n');
    out
}

pub(crate) fn format_invariant(inv: &Invariant, ctx: FormatContext) -> String {
    let mut out = String::new();
    // The surface has no version syntax, so `version` is omitted.
    // The totality declaration is authored, so it is printed.
    match &inv.totality_for {
        Some(p) => out.push_str(&format!("invariant {} total over {p}:\n", inv.name)),
        None => out.push_str(&format!("invariant {}:\n", inv.name)),
    }
    out.push_str(&indent(1));
    out.push_str(&fmt_prop(&inv.body, ctx));
    out.push('\n');
    out
}

pub(crate) fn format_transformation(t: &Transformation, ctx: FormatContext) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "transformation {}({}):\n",
        t.name,
        t.parameters
            .iter()
            .map(Var::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    ));
    for stmt in &t.body {
        out.push_str(&fmt_stmt(stmt, 1, ctx));
        out.push('\n');
    }
    out
}

pub(crate) fn format_derived_claim(d: &DerivedClaim, ctx: FormatContext) -> String {
    // The grammar requires at least one `value` clause. Panic rather
    // than emit text the parser refuses.
    assert!(
        !d.values.is_empty(),
        "format_derived_claim: derived claim `{}` has no values; the surface grammar requires at least one `value` clause",
        d.predicate,
    );
    let mut out = String::new();
    out.push_str(&format!(
        "derived {}({}):\n",
        d.predicate,
        d.keys
            .iter()
            .map(Var::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push_str(&indent(1));
    out.push_str(&format!("over {}\n", fmt_prop(&d.domain, ctx)));
    for v in &d.values {
        out.push_str(&indent(1));
        out.push_str(&format!("value {} = {}\n", v.name, fmt_value(&v.expr, ctx)));
    }
    out
}

// ============================================================
// Statement formatting
// ============================================================

/// `name: ` when a gate has a name, empty otherwise.
fn rule_label(name: &Option<crate::ir::RuleName>) -> String {
    name.as_ref().map_or_else(String::new, |n| format!("{n}: "))
}

fn fmt_stmt(s: &Stmt, depth: usize, ctx: FormatContext) -> String {
    let pad = indent(depth);
    match s {
        Stmt::Require { prop: p, name } => {
            format!("{pad}require {}{}", rule_label(name), fmt_prop(p, ctx))
        }
        Stmt::BindOne { prop: p, name } => {
            // A claim pattern or a definition call; both render as
            // `Name(args)` and reparse to themselves. The parser cannot
            // produce anything else, so nothing else would read back.
            assert!(
                matches!(p, Prop::Claim { .. } | Prop::Defined { .. }),
                "format_stmt: bind takes a claim or a defined call; got {p:?}",
            );
            format!("{pad}bind {}{}", rule_label(name), fmt_prop(p, ctx))
        }
        Stmt::Let { name, value } => {
            format!("{pad}let {name} = {}", fmt_value(value, ctx))
        }
        Stmt::LetNewSubject { name } => {
            format!("{pad}let {name} = new Subject()")
        }
        Stmt::Assert(c) => format!("{pad}admit {}", format_claim(c)),
        Stmt::Retract { predicate, args } => {
            format!(
                "{pad}retract {}",
                fmt_predicate_call(predicate.as_str(), args, ctx)
            )
        }
        Stmt::Emit(i) => format!("{pad}emit {}", format_intent(i)),
        Stmt::For {
            binding,
            collection,
            body,
        } => {
            let mut out = format!("{pad}for {binding} in {}:\n", fmt_value(collection, ctx));
            for (i, inner) in body.iter().enumerate() {
                out.push_str(&fmt_stmt(inner, depth + 1, ctx));
                if i + 1 < body.len() {
                    out.push('\n');
                }
            }
            out
        }
    }
}

// ============================================================
// Expression formatting
// ============================================================

/// One-line rendering of a [`Prop`], for kernel diagnostics (rejection
/// reasons, multi-match errors) and read-side text (`inspect controls`,
/// `guarantees`).
///
/// **Display only, not source.** Without a declaration table, a `value`
/// lookup whose hole is not its first wildcard prints positionally, which
/// would reparse differently. Use [`format_prop_source`] and its
/// siblings for source.
pub fn format_prop_inline(p: &Prop) -> String {
    fmt_prop(p, FormatContext::DIAGNOSTIC)
}

fn fmt_prop(p: &Prop, ctx: FormatContext) -> String {
    // Composite sub-propositions are always parenthesised: verbose, but
    // unambiguous, and harmless to the parser.
    fn prop_primary(p: &Prop, ctx: FormatContext) -> String {
        match p {
            Prop::Claim { predicate, args } => fmt_predicate_call(predicate.as_str(), args, ctx),
            // A definition call has parameters, not fields, so it never
            // takes the named form. Branch on the variant: a definition
            // may share its name with a predicate.
            Prop::Defined { name, args } => {
                fmt_predicate_call(name.as_str(), args, FormatContext::DIAGNOSTIC)
            }
            // `pre(...)` is function-call-shape; no outer parens needed.
            Prop::Pre(inner) => format!("pre({})", fmt_prop(inner, ctx)),
            _ => format!("({})", fmt_prop(p, ctx)),
        }
    }

    match p {
        Prop::Claim { predicate, args } => fmt_predicate_call(predicate.as_str(), args, ctx),
        // Renders like a claim; the parser resolves it by name. Never in
        // named form - see `prop_primary`.
        Prop::Defined { name, args } => {
            fmt_predicate_call(name.as_str(), args, FormatContext::DIAGNOSTIC)
        }

        // Comparators relate two value expressions.
        Prop::Eq(l, r) => format!("{} = {}", value_primary(l, ctx), value_primary(r, ctx)),
        Prop::Compare {
            op,
            domain,
            left,
            right,
        } => format!(
            "{} {} {}",
            value_primary(left, ctx),
            compare_token(*op, *domain),
            value_primary(right, ctx)
        ),
        Prop::Neq(lhs, rhs) => {
            format!("{} != {}", value_primary(lhs, ctx), value_primary(rhs, ctx))
        }
        Prop::In(elem, coll) => format!("{} in {}", format_term(elem), format_term(coll)),

        // Boolean composition: prefix `not`, infix `and`/`or`/`implies`.
        Prop::Pre(inner) => format!("pre({})", fmt_prop(inner, ctx)),
        Prop::Not(inner) => format!("not {}", prop_primary(inner, ctx)),
        Prop::And(props) => {
            let inner: Vec<String> = props.iter().map(|p| prop_primary(p, ctx)).collect();
            inner.join(" and ")
        }
        Prop::Or(props) => {
            let inner: Vec<String> = props.iter().map(|p| prop_primary(p, ctx)).collect();
            inner.join(" or ")
        }
        Prop::Xor(left, right) => {
            format!(
                "{} xor {}",
                prop_primary(left, ctx),
                prop_primary(right, ctx)
            )
        }
        Prop::Implies { left, right } => {
            format!(
                "{} implies {}",
                prop_primary(left, ctx),
                prop_primary(right, ctx)
            )
        }

        // Quantifiers: colon-block form.
        Prop::Exists { binding, body } => {
            format!("exists {binding}: {}", fmt_prop(body, ctx))
        }
        Prop::Forall {
            binding,
            source,
            body,
        } => {
            // The parser builds `forall x in coll:` as
            // `Prop::In(Term::Var(x), coll)`; print that shape back the
            // same way, and any other source as a proposition.
            let source_text = match source.as_ref() {
                Prop::In(Term::Var(b), coll) if b == binding => format_term(coll),
                _ => prop_primary(source, ctx),
            };
            format!("forall {binding} in {source_text}: {}", fmt_prop(body, ctx))
        }
    }
}

/// Render a value expression for an operand position. Infix arithmetic
/// is the only ambiguous form, so it alone is parenthesised; everything
/// else is self-delimiting.
fn value_primary(e: &ValueExpr, ctx: FormatContext) -> String {
    match e {
        ValueExpr::Term(t) => format_term(t),
        ValueExpr::Arith { .. } => {
            format!("({})", fmt_value(e, ctx))
        }
        _ => fmt_value(e, ctx),
    }
}

/// Inline rendering that reparses to the same IR. Like the `_inline`
/// helpers, but with the programme's declaration table, so a `value`
/// lookup whose hole is not its first wildcard prints in named form.
/// Use these for source output (`check --ir`); `_inline` is for display.
pub fn format_prop_source(p: &Program, prop: &Prop) -> String {
    let naming = claim_naming(p);
    fmt_prop(
        prop,
        FormatContext {
            predicates: Some(&naming),
            named_canonical: false,
        },
    )
}

/// See [`format_prop_source`].
pub fn format_value_source(p: &Program, e: &ValueExpr) -> String {
    let naming = claim_naming(p);
    fmt_value(
        e,
        FormatContext {
            predicates: Some(&naming),
            named_canonical: false,
        },
    )
}

/// See [`format_prop_source`].
pub fn format_stmt_source(p: &Program, s: &Stmt, depth: usize) -> String {
    let naming = claim_naming(p);
    fmt_stmt(
        s,
        depth,
        FormatContext {
            predicates: Some(&naming),
            named_canonical: false,
        },
    )
}

fn fmt_value(e: &ValueExpr, ctx: FormatContext) -> String {
    match e {
        ValueExpr::Term(t) => format_term(t),
        ValueExpr::Sum {
            value,
            body,
            seed: _,
        } => {
            format!("sum({} | {})", fmt_value(value, ctx), fmt_prop(body, ctx))
        }
        ValueExpr::Extremum { op, value, body } => {
            format!(
                "{}({} | {})",
                op.as_str(),
                format_term(value),
                fmt_prop(body, ctx)
            )
        }
        ValueExpr::ValueOf {
            predicate,
            args,
            extract,
            default,
        } => {
            // Not a style choice. If the hole is the first wildcard,
            // positional text reparses to this IR, so it stays positional.
            // Otherwise positional would pick the wrong hole, so it prints
            // named. Without a table (only in invalid IR) it falls back
            // to positional.
            let first_wildcard = args.iter().position(|t| matches!(t, Term::Wildcard));
            let base = if first_wildcard == Some(*extract) || first_wildcard.is_none() {
                format!(
                    "value {}",
                    fmt_predicate_call(predicate.as_str(), args, FormatContext::DIAGNOSTIC)
                )
            } else {
                format!(
                    "value {}",
                    named_value_lookup(predicate.as_str(), args, *extract, ctx)
                )
            };
            match default {
                Some(d) => format!("{base} default {}", fmt_value(d, ctx)),
                None => base,
            }
        }
        ValueExpr::Arith { op, left, right } => {
            format!(
                "{} {} {}",
                value_primary(left, ctx),
                arith_token(*op),
                value_primary(right, ctx)
            )
        }
        // Every builtin renders as `name(args)`.
        ValueExpr::Call { builtin, args } => format!(
            "{}({})",
            builtin.name(),
            args.iter()
                .map(|a| fmt_value(a, ctx))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        // Function-shaped, like a builtin.
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => format!(
            "if({}, {}, {})",
            fmt_prop(when, ctx),
            fmt_value(then, ctx),
            fmt_value(otherwise, ctx)
        ),
    }
}

// ============================================================
// Leaf formatting
// ============================================================

/// The named spelling of a `value` lookup whose hole is not its first
/// wildcard: the hole as `field: _`, constrained fields in declaration
/// order, the rest behind `..`. Falls back to positional when the table
/// cannot resolve the predicate, which only invalid hand-built IR does.
fn named_value_lookup(
    predicate: &str,
    args: &[Term],
    extract: usize,
    ctx: FormatContext,
) -> String {
    if let Some(map) = ctx.predicates
        && let Some(decl) = map.get(predicate)
        && decl.args.len() == args.len()
        && matches!(args.get(extract), Some(Term::Wildcard))
    {
        let mut elided = false;
        let mut parts: Vec<String> = Vec::with_capacity(args.len());
        for (position, (field, term)) in decl.args.iter().zip(args).enumerate() {
            if position == extract {
                parts.push(format!("{}: _", field.name));
            } else if matches!(term, Term::Wildcard) {
                elided = true;
            } else {
                parts.push(format!("{}: {}", field.name, format_term(term)));
            }
        }
        if elided {
            parts.push("..".to_string());
        }
        return format!("{predicate}({})", parts.join(", "));
    }
    fmt_predicate_call(predicate, args, FormatContext::DIAGNOSTIC)
}

fn fmt_predicate_call(predicate: &str, args: &[Term], ctx: FormatContext) -> String {
    // Two or more wildcards in a row are hard to read, so name the
    // mentioned fields and write the rest as `..`. An all-wildcard
    // pattern becomes `Pred(..)`.
    if ctx.named_canonical
        && let Some(map) = ctx.predicates
        && let Some(decl) = map.get(predicate)
        && decl.args.len() == args.len()
        && max_wildcard_run(args) >= 2
    {
        let mut parts: Vec<String> = decl
            .args
            .iter()
            .zip(args)
            .filter(|(_, term)| !matches!(term, Term::Wildcard))
            .map(|(field, term)| format!("{}: {}", field.name, format_term(term)))
            .collect();
        parts.push("..".to_string());
        return format!("{predicate}({})", parts.join(", "));
    }
    let formatted: Vec<String> = args.iter().map(format_term).collect();
    format!("{predicate}({})", formatted.join(", "))
}

fn format_claim(c: &Claim) -> String {
    // `admit` supplies every field, so there are no wildcards.
    fmt_predicate_call(c.predicate.as_str(), &c.args, FormatContext::DIAGNOSTIC)
}

fn format_intent(i: &Intent) -> String {
    fmt_predicate_call(i.name.as_str(), &i.args, FormatContext::DIAGNOSTIC)
}

fn format_term(t: &Term) -> String {
    match t {
        Term::Var(name) => name.to_string(),
        Term::Wildcard => "_".to_string(),
        Term::Literal(v) => format_value(v),
        Term::Actor => "actor".to_string(),
    }
}

fn format_value(v: &Value) -> String {
    match v {
        // `#name`. The lexer accepts only an ASCII identifier after `#`,
        // so panic now on a subject that would not round-trip.
        Value::Subject(s) => {
            let s = s.as_str();
            assert!(
                is_identifier_safe_subject(s),
                "format_value: Value::Subject({s:?}) is not identifier-safe; the `#name` surface accepts only ASCII identifiers",
            );
            format!("#{s}")
        }
        Value::Decimal(s) => s.clone(),
        // Date literals use the @YYYY-MM-DD sigil.
        Value::Date(s) => format!("@{s}"),
        // Timestamp literals extend the same sigil to a full RFC 3339
        // instant: @2026-10-24T14:00:00Z.
        Value::Timestamp(s) => format!("@{s}"),
        // A constructor form; no quotes, as the surface has no strings.
        Value::Duration(s) => format!("duration({s})"),
        // Same shape as durations; the source string round-trips.
        Value::CalendarSpan(s) => format!("span({s})"),
        // Amount then unit, as an invoice writes them.
        Value::Quantity { amount, unit } => format!("{amount} {unit}"),
    }
}

/// `#<ident>` accepts ASCII letters / digits / underscore, with a
/// non-digit first character. Matches the surface lexer's
/// subject-literal production.
fn is_identifier_safe_subject(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

#[cfg(test)]
mod tests {
    //! Tests check the tokens the output must contain, not exact bytes
    //! unless a test says so, and that every variant renders.

    use super::*;
    use crate::ir_builder::*;
    use crate::{PredicateArgKind, Value};

    /// The decl-free diagnostic renderings, for the shape pins below.
    fn fmt_value_diag(e: &ValueExpr) -> String {
        fmt_value(e, FormatContext::DIAGNOSTIC)
    }

    fn fmt_stmt_diag(s: &Stmt, depth: usize) -> String {
        fmt_stmt(s, depth, FormatContext::DIAGNOSTIC)
    }

    #[test]
    fn format_program_starts_with_program_header() {
        let p = program("demo").build();
        let s = format_program(&p);
        assert!(s.starts_with("program demo"));
    }

    #[test]
    fn format_transformation_shows_parameter_list_and_body_indented() {
        let t = transformation(
            "open_trial",
            params(&["trial_id"]),
            vec![
                assert_("Trial", vec![var("trial_id")]),
                emit("TrialOpened", vec![var("trial_id")]),
            ],
        );
        let s = format_transformation(&t, FormatContext::DIAGNOSTIC);
        assert!(s.contains("transformation open_trial(trial_id):"));
        assert!(s.contains("  admit Trial(trial_id)"));
        assert!(s.contains("  emit TrialOpened(trial_id)"));
    }

    /// `Stmt::BindOne` renders as `bind <expr>`, like `require <expr>`.
    #[test]
    fn format_stmt_renders_bind_one_with_inline_expression() {
        let s = fmt_stmt_diag(
            &bind_one(claim("Policy", vec![var("policy_id"), var("limit")])),
            1,
        );
        assert_eq!(s, "  bind Policy(policy_id, limit)");
    }

    /// Predicate declarations render between the header and the
    /// invariants section, one per line, with no blank line between
    /// consecutive declarations. Argument kinds render with their
    /// PascalCase names (`Subject`, `Decimal`, `Date`, etc.).
    #[test]
    fn format_predicate_decl_renders_inline_with_typed_args() {
        let decl = predicate("Policy")
            .subject("policy_id")
            .decimal("aggregate_limit")
            .build();
        let s = format_predicate_decl(&decl);
        assert_eq!(
            s,
            "predicate Policy(policy_id: Subject, aggregate_limit: Decimal)\n"
        );
    }

    /// The predicate section in `format_program`: one blank line after
    /// the header, then declarations with no blank lines between.
    #[test]
    fn format_program_renders_predicates_section_consecutively() {
        let p = program("tiny")
            .predicates(vec![
                predicate("Foo").subject("a").build(),
                predicate("Bar").decimal("n").build(),
            ])
            .build();
        let s = format_program(&p);
        // Exact bytes: header, blank line, two predicate lines, nothing else.
        assert_eq!(
            s,
            "program tiny\n\npredicate Foo(a: Subject)\npredicate Bar(n: Decimal)\n"
        );
    }

    /// Every `PredicateArgKind` has a stable display name, shared by the
    /// formatter and validation errors, so the unit always renders
    /// (`Decimal[USD]`).
    #[test]
    fn format_predicate_arg_kind_renders_each_variant() {
        for (kind, expected) in [
            (PredicateArgKind::Subject, "Subject"),
            (PredicateArgKind::Decimal, "Decimal"),
            (PredicateArgKind::Date, "Date"),
            (PredicateArgKind::Bool, "Bool"),
            (PredicateArgKind::Collection, "Collection"),
            (PredicateArgKind::Any, "Any"),
        ] {
            assert_eq!(kind.to_string(), expected);
        }
    }

    #[test]
    fn format_prop_renders_each_variant() {
        // One proposition using every Prop variant; each must produce a
        // recognisable token. Comparator operands also reach the value
        // renderer.
        let p = and(vec![
            claim("P", vec![var("x"), wildcard()]),
            not(claim("Q", vec![var("x")])),
            implies(
                claim("R", vec![var("y")]),
                claim("S", vec![var("y"), actor()]),
            ),
            exists("z", claim("T", vec![var("z")])),
            forall("w", claim("U", vec![var("w")]), claim("V", vec![var("w")])),
            eq(term(var("a")), term(var("b"))),
            neq(var("a"), var("b")),
            le(add(term(var("a")), term(var("c"))), term(var("b"))),
            date_le(term(var("d1")), term(var("d2"))),
            in_(var("e"), var("coll")),
        ]);
        let s = format_prop_inline(&p);

        assert!(s.contains("P(x, _)"));
        assert!(s.contains("not Q(x)"));
        assert!(s.contains("implies"));
        assert!(s.contains("exists z:"));
        assert!(s.contains("forall w in"));
        assert!(s.contains("a = b"));
        assert!(s.contains("a != b"));
        assert!(s.contains("(a + c) <= b"));
        assert!(s.contains("d1 on_or_before d2"));
        assert!(s.contains("e in coll"));
        assert!(s.contains("actor"));
    }

    #[test]
    fn format_value_renders_each_variant() {
        // One value expression using every ValueExpr variant; each must
        // produce a recognisable token.
        let e = add(
            sub(term(var("p")), term(var("q"))),
            sum(var("v"), claim("W", vec![var("v")])),
        );
        let s = fmt_value_diag(&e);
        assert!(s.contains("p - q"));
        assert!(s.contains("sum(v |"));

        let vo = value_of("X", vec![var("k"), wildcard()]);
        assert!(fmt_value_diag(&vo).contains("value X(k, _)"));

        // Mul, Div, Min, Max - including the nested collar shape
        // `min(_, max(0, _))`; min/max are self-delimiting, so their
        // operands render without extra parens.
        let collar = min(
            mul(term(var("a")), term(var("b"))),
            max(term(dec("0")), div(term(var("c")), term(var("d")))),
        );
        let printed = fmt_value_diag(&collar);
        assert_eq!(printed, "min(a * b, max(0, c / d))");

        // Mod renders infix `%` and parenthesises inside another operand,
        // like the other infix arithmetic: the chess parity shape.
        let parity = modulo(add(term(var("f")), term(var("r"))), term(dec("2")));
        assert_eq!(fmt_value_diag(&parity), "(f + r) % 2");
    }

    #[test]
    fn format_term_renders_literals_subject_decimal_date() {
        assert_eq!(
            format_term(&Term::Literal(Value::Subject("foo".into()))),
            "#foo"
        );
        assert_eq!(
            format_term(&Term::Literal(Value::Decimal("1250.75".to_string()))),
            "1250.75"
        );
        assert_eq!(
            format_term(&Term::Literal(Value::Date("2026-03-12".to_string()))),
            "@2026-03-12"
        );
        assert_eq!(format_term(&Term::Wildcard), "_");
        assert_eq!(format_term(&Term::Actor), "actor");
        assert_eq!(format_term(&Term::Var("x".into())), "x");
    }

    /// `format_program` output ends with a trailing newline.
    #[test]
    fn format_program_output_ends_with_newline() {
        let p = program("demo")
            .transformations(vec![transformation("noop", vec![], vec![])])
            .build();
        let s = format_program(&p);
        assert!(s.ends_with('\n'), "expected trailing newline; got: {s:?}");
    }
}
