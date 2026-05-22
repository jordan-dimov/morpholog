//! Human-readable pretty-printer for [`Program`]s and their components.
//!
//! The IR types derive `Debug`, which is fine for `cargo test` failure
//! messages and adequate for one-off inspection but is unreadable for
//! anything more than a few claims deep. This module renders the same
//! IR in a structured indented form intended for:
//!
//! - A failing test's `panic!` message that needs to show *which*
//!   transformation produced unexpected state, not just dump the whole
//!   `Outcome`.
//! - The CLI's `inspect program` subcommand (when it lands) - operators
//!   reading `morpholog inspect program <name>` should see what the
//!   programme actually does, not a Debug-formatted dump.
//! - A `diff` between two `Program` values - the `.morph` illustrative
//!   file in each example directory currently has no machine-checkable
//!   relationship to the Rust IR; this printer is the bridge.
//!
//! Output style:
//!
//! - **Not future surface syntax.** `.morph` is the eventual parser
//!   target; the parser will define the surface. This printer aims for
//!   *readable now* with 2-space indentation and no commitment to any
//!   specific token choices.
//! - **One concept per line where possible.** Statement bodies expand
//!   vertically; sub-expressions inline unless they contain claims or
//!   `And`/`Or`-style branching.
//! - **Result ends with a trailing newline** so callers can append
//!   directly to a `String` or write to a stream without composing
//!   their own separator. Each per-section helper
//!   (`format_invariant`, `format_transformation`,
//!   `format_derived_claim`) likewise produces a newline-terminated
//!   block.
//!
//! Cost: a deliberate cost. The exhaustive matches in this module
//! mean every new IR variant ([`Expr`], [`Stmt`], [`Term`], [`Value`])
//! requires a new arm. That is the same discipline as
//! [`crate::predicates_referenced_by_expr`]: a compile-time gate
//! that no future addition silently degrades the human-readable
//! rendering.

use crate::{
    Claim, DerivedClaim, Expr, Intent, Invariant, PredicateArgKind, PredicateDecl, Program, Stmt,
    Term, Transformation, Value,
};

/// Top-level entry. Returns a multi-line string terminated by a
/// final `\n`, so callers can write directly to a stream or append
/// to an existing buffer.
pub fn format_program(p: &Program) -> String {
    let mut out = String::new();
    out.push_str(&format!("program {}\n", p.name));

    // Predicates render between the header and the invariants - they
    // are the programme's vocabulary contract, and seeing them first
    // helps the reader interpret every subsequent claim reference.
    // One blank line separates the section from the header; the
    // declarations themselves stack consecutively (each
    // format_predicate_decl call ends with its own `\n`).
    if !p.predicates.is_empty() {
        out.push('\n');
        for decl in &p.predicates {
            out.push_str(&format_predicate_decl(decl));
        }
    }

    for inv in &p.invariants {
        out.push('\n');
        out.push_str(&format_invariant(inv));
    }

    for t in &p.transformations {
        out.push('\n');
        out.push_str(&format_transformation(t));
    }

    for d in &p.derived_claims {
        out.push('\n');
        out.push_str(&format_derived_claim(d));
    }

    out
}

/// Render a single [`PredicateDecl`] as one line:
/// `predicate Name(arg1: Kind, arg2: Kind)`.
pub fn format_predicate_decl(decl: &PredicateDecl) -> String {
    let args: Vec<String> = decl
        .args
        .iter()
        .map(|a| format!("{}: {}", a.name, format_predicate_arg_kind(a.kind)))
        .collect();
    format!("predicate {}({})\n", decl.name, args.join(", "))
}

fn format_predicate_arg_kind(k: PredicateArgKind) -> &'static str {
    match k {
        PredicateArgKind::Subject => "Subject",
        PredicateArgKind::Decimal => "Decimal",
        PredicateArgKind::Date => "Date",
        PredicateArgKind::Bool => "Bool",
        PredicateArgKind::Collection => "Collection",
        PredicateArgKind::Any => "Any",
    }
}

pub fn format_invariant(inv: &Invariant) -> String {
    let mut out = String::new();
    // Surface has no version syntax in v0; the IR's `version` field
    // defaults to 1 and the formatter omits it. When versioning
    // grows a meaningful second value, both the surface and this
    // emitter add a clause.
    out.push_str(&format!("invariant {}:\n", inv.name));
    out.push_str(&indent(1));
    out.push_str(&format_expr_inline(&inv.body));
    out.push('\n');
    out
}

pub fn format_transformation(t: &Transformation) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "transformation {}({}):\n",
        t.name,
        t.parameters.join(", ")
    ));
    for stmt in &t.body {
        out.push_str(&format_stmt(stmt, 1));
        out.push('\n');
    }
    out
}

pub fn format_derived_claim(d: &DerivedClaim) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "derived {}({}):\n",
        d.predicate,
        d.keys.join(", ")
    ));
    out.push_str(&indent(1));
    out.push_str(&format!("over {}\n", format_expr_inline(&d.domain)));
    for v in &d.values {
        out.push_str(&indent(1));
        out.push_str(&format!(
            "value {} = {}\n",
            v.name,
            format_expr_inline(&v.expr)
        ));
    }
    out
}

// ============================================================
// Statement formatting
// ============================================================

pub fn format_stmt(s: &Stmt, depth: usize) -> String {
    let pad = indent(depth);
    match s {
        Stmt::Require(e) => format!("{pad}require {}", format_expr_inline(e)),
        Stmt::BindOne(e) => format!("{pad}bind {}", format_expr_inline(e)),
        Stmt::Let { name, value } => {
            format!("{pad}let {name} = {}", format_expr_inline(value))
        }
        Stmt::LetNewSubject { name } => {
            format!("{pad}let {name} = new Subject()")
        }
        Stmt::Assert(c) => format!("{pad}admit {}", format_claim(c)),
        Stmt::Retract { predicate, args } => {
            format!("{pad}retract {}", format_predicate_call(predicate, args))
        }
        Stmt::Emit(i) => format!("{pad}emit {}", format_intent(i)),
        Stmt::For {
            binding,
            collection,
            body,
        } => {
            let mut out = format!(
                "{pad}for {binding} in {}:\n",
                format_expr_inline(collection)
            );
            for (i, inner) in body.iter().enumerate() {
                out.push_str(&format_stmt(inner, depth + 1));
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

/// Indented multi-line expression. Used by invariant bodies, derived-
/// claim domains, and `For` bodies where vertical layout aids reading.
/// One-line expression rendering. The only expression printer in
/// the kernel; produces canonical surface text that round-trips
/// through `parse_program`. Used inline in `require`, `let`,
/// `bind`, invariant bodies, derived-claim domain and value
/// expressions, and kernel diagnostic paths (`bind` rejection
/// reasons, multi-match errors) that need a compact human-readable
/// rendering
/// of an expression.
pub fn format_expr_inline(e: &Expr) -> String {
    // Emits canonical surface text. Composite sub-expressions are
    // wrapped in parens unconditionally; the result is verbose but
    // unambiguous and round-trips through `parse_program`. The
    // surface comparator precedence (arithmetic > comparators > not
    // > and > implies) makes the parens a no-op for the parser.
    fn primary(e: &Expr) -> String {
        match e {
            Expr::Term(t) => format_term(t),
            Expr::Claim { predicate, args } => format_predicate_call(predicate, args),
            Expr::Sum {
                value: _,
                binding,
                body,
            } => format!("sum({binding} | {})", format_expr_inline(body)),
            Expr::ValueOf {
                predicate,
                args,
                default,
            } => {
                let base = format!("value {}", format_predicate_call(predicate, args));
                match default {
                    Some(d) => format!("{base} default {}", format_expr_inline(d)),
                    None => base,
                }
            }
            // Any composite gets parens.
            _ => format!("({})", format_expr_inline(e)),
        }
    }

    match e {
        Expr::Term(t) => format_term(t),
        Expr::Claim { predicate, args } => format_predicate_call(predicate, args),
        Expr::Sum {
            value: _,
            binding,
            body,
        } => format!("sum({binding} | {})", format_expr_inline(body)),
        Expr::ValueOf {
            predicate,
            args,
            default,
        } => {
            let base = format!("value {}", format_predicate_call(predicate, args));
            match default {
                Some(d) => format!("{base} default {}", format_expr_inline(d)),
                None => base,
            }
        }

        // Arithmetic and comparators: operands are primary-shaped.
        Expr::Add(l, r) => format!("{} + {}", primary(l), primary(r)),
        Expr::Sub(l, r) => format!("{} - {}", primary(l), primary(r)),
        Expr::Eq(l, r) => format!("{} = {}", primary(l), primary(r)),
        Expr::Le(l, r) => format!("{} <= {}", primary(l), primary(r)),
        Expr::DateLe(l, r) => format!("{} on_or_before {}", primary(l), primary(r)),
        Expr::Neq(t1, t2) => format!("{} != {}", format_term(t1), format_term(t2)),
        Expr::In(elem, coll) => format!("{} in {}", format_term(elem), format_term(coll)),

        // Boolean composition: prefix `not`, infix `and`, infix `implies`.
        Expr::Not(inner) => format!("not {}", primary(inner)),
        Expr::And(exprs) => {
            let inner: Vec<String> = exprs.iter().map(primary).collect();
            inner.join(" and ")
        }
        Expr::Implies { left, right } => {
            format!("{} implies {}", primary(left), primary(right))
        }

        // Quantifiers: colon-block form. Source for `forall` is a
        // primary expression (typically a Claim or bare Var).
        Expr::Exists { binding, body } => {
            format!("exists {binding}: {}", format_expr_inline(body))
        }
        Expr::Forall {
            binding,
            source,
            body,
        } => {
            // The IR's source is an Expr; the natural surface
            // form `forall x in coll:` is built by the parser as
            // `Expr::In(Term::Var(x), coll)`. Detect that lifted
            // shape and emit the natural surface; otherwise fall
            // back to whatever primary expression the source is.
            let source_text = match source.as_ref() {
                Expr::In(Term::Var(b), coll) if b == binding => format_term(coll),
                _ => primary(source),
            };
            format!(
                "forall {binding} in {source_text}: {}",
                format_expr_inline(body)
            )
        }
    }
}

// ============================================================
// Leaf formatting
// ============================================================

fn format_predicate_call(predicate: &str, args: &[Term]) -> String {
    let formatted: Vec<String> = args.iter().map(format_term).collect();
    format!("{predicate}({})", formatted.join(", "))
}

fn format_claim(c: &Claim) -> String {
    format_predicate_call(&c.predicate, &c.args)
}

fn format_intent(i: &Intent) -> String {
    format_predicate_call(&i.name, &i.args)
}

fn format_term(t: &Term) -> String {
    match t {
        Term::Var(name) => name.clone(),
        Term::Wildcard => "_".to_string(),
        Term::Literal(v) => format_value(v),
        Term::Actor => "actor".to_string(),
    }
}

fn format_value(v: &Value) -> String {
    match v {
        // Subjects use the `#name` sigil. Subject literals in worked
        // examples are always identifier-safe (ASCII letters, digits,
        // underscore); if a non-identifier subject ever needs
        // rendering, the formatter will round-trip incorrectly and
        // the round-trip test will catch it.
        Value::Subject(s) => format!("#{s}"),
        Value::Decimal(s) => s.clone(),
        // Date literals use the @YYYY-MM-DD sigil.
        Value::Date(s) => format!("@{s}"),
    }
}

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Tests pin the *shape* of the rendered output - specific tokens
    //! the rendering must contain - not byte-for-byte equality. The
    //! exact whitespace and indentation may change as the printer is
    //! tuned; what must not change is that every variant is reachable
    //! and produces readable output.

    use super::*;
    use crate::Value;
    use crate::dsl::*;

    #[test]
    fn format_program_starts_with_program_header() {
        let p = Program {
            predicates: vec![],
            name: "demo".to_string(),
            invariants: vec![],
            transformations: vec![],
            derived_claims: vec![],
        };
        let s = format_program(&p);
        assert!(s.starts_with("program demo"));
    }

    #[test]
    fn format_transformation_shows_parameter_list_and_body_indented() {
        let t = Transformation {
            name: "open_trial".to_string(),
            parameters: params(&["trial_id"]),
            body: vec![
                assert_("Trial", vec![var("trial_id")]),
                emit("TrialOpened", vec![var("trial_id")]),
            ],
        };
        let s = format_transformation(&t);
        assert!(s.contains("transformation open_trial(trial_id):"));
        assert!(s.contains("  admit Trial(trial_id)"));
        assert!(s.contains("  emit TrialOpened(trial_id)"));
    }

    /// `Stmt::BindOne` renders as `bind <expr>` with the inner
    /// expression formatted inline. Mirrors the `require <expr>`
    /// shape; the two read in parallel in any pretty-printed
    /// transformation body.
    #[test]
    fn format_stmt_renders_bind_one_with_inline_expression() {
        let s = format_stmt(
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

    /// Pins the predicate section layout in `format_program`: one
    /// blank line separates the section from the header, then
    /// declarations stack consecutively with no intervening blank
    /// lines. Two consecutive predicates rendered with an extra blank
    /// line between them was the Copilot review finding on PR #50.
    #[test]
    fn format_program_renders_predicates_section_consecutively() {
        let p = Program {
            name: "tiny".to_string(),
            predicates: vec![
                predicate("Foo").subject("a").build(),
                predicate("Bar").decimal("n").build(),
            ],
            invariants: vec![],
            transformations: vec![],
            derived_claims: vec![],
        };
        let s = format_program(&p);
        // Exact bytes: header, blank line, two predicate lines, nothing else.
        assert_eq!(
            s,
            "program tiny\n\npredicate Foo(a: Subject)\npredicate Bar(n: Decimal)\n"
        );
    }

    /// Every `PredicateArgKind` variant has a stable display name in
    /// the formatter. Exhaustive match means a future variant
    /// (e.g. an `Instant` kind when timezone-aware values land) must
    /// extend `format_predicate_arg_kind`.
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
            assert_eq!(format_predicate_arg_kind(kind), expected);
        }
    }

    #[test]
    fn format_expr_renders_each_variant() {
        // One expression exercising every Expr variant. The test pins
        // that each variant produces a recognisable token in the
        // output; if a future variant is added without a printer arm,
        // the exhaustive match in `format_expr_inline` will refuse to
        // compile.
        let e = and(vec![
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
            le(term(var("a")), term(var("b"))),
            date_le(term(var("d1")), term(var("d2"))),
            add(term(var("p")), term(var("q"))),
            sub(term(var("p")), term(var("q"))),
            sum(var("v"), "v", claim("W", vec![var("v")])),
            in_(var("e"), var("coll")),
            value_of("X", vec![var("k"), wildcard()]),
        ]);
        let s = format_expr_inline(&e);

        // Each variant contributes at least one recognisable token
        // matching the surface syntax that round-trips through the
        // parser.
        assert!(s.contains("P(x, _)"));
        assert!(s.contains("not Q(x)"));
        assert!(s.contains("implies"));
        assert!(s.contains("exists z:"));
        assert!(s.contains("forall w in"));
        assert!(s.contains("a = b"));
        assert!(s.contains("a != b"));
        assert!(s.contains("a <= b"));
        assert!(s.contains("d1 on_or_before d2"));
        assert!(s.contains("p + q"));
        assert!(s.contains("p - q"));
        assert!(s.contains("sum(v |"));
        assert!(s.contains("e in coll"));
        assert!(s.contains("value X(k, _)"));
        assert!(s.contains("actor"));
    }

    #[test]
    fn format_term_renders_literals_subject_decimal_date() {
        assert_eq!(
            format_term(&Term::Literal(Value::Subject("foo".to_string()))),
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
        assert_eq!(format_term(&Term::Var("x".to_string())), "x");
    }

    /// `format_program` documents that its output ends with a
    /// trailing newline. Pin that contract so callers can rely on it
    /// (write directly to a stream, append without composing a
    /// separator).
    #[test]
    fn format_program_output_ends_with_newline() {
        let p = Program {
            predicates: vec![],
            name: "demo".to_string(),
            invariants: vec![],
            transformations: vec![Transformation {
                name: "noop".to_string(),
                parameters: vec![],
                body: vec![],
            }],
            derived_claims: vec![],
        };
        let s = format_program(&p);
        assert!(s.ends_with('\n'), "expected trailing newline; got: {s:?}");
    }
}
