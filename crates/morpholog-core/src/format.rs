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
//! - **No trailing newline on the result.** The caller composes.
//!
//! Cost: a deliberate cost. The exhaustive matches in this module
//! mean every new IR variant ([`Expr`], [`Stmt`], [`Term`], [`Value`])
//! requires a new arm. That is the same discipline as
//! [`crate::predicates_referenced_by_expr`]: a compile-time gate
//! that no future addition silently degrades the human-readable
//! rendering.

use crate::{
    Claim, DerivedClaim, Expr, Intent, Invariant, Program, Stmt, Term, Transformation, Value,
};

/// Top-level entry. Returns a multi-line string ready to `println!`
/// (no trailing newline added).
pub fn format_program(p: &Program) -> String {
    let mut out = String::new();
    out.push_str(&format!("program {}\n", p.name));

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

pub fn format_invariant(inv: &Invariant) -> String {
    let mut out = String::new();
    out.push_str(&format!("invariant {} (v{}):\n", inv.name, inv.version));
    out.push_str(&format_expr(&inv.body, 1));
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
    if !d.values.is_empty() {
        out.push_str(&indent(1));
        out.push_str("values:\n");
        for v in &d.values {
            out.push_str(&indent(2));
            out.push_str(&format!("{} = ", v.name));
            out.push_str(&format_expr_inline(&v.expr));
            out.push('\n');
        }
    }
    out.push_str(&indent(1));
    out.push_str("over\n");
    out.push_str(&format_expr(&d.domain, 2));
    out.push('\n');
    out
}

// ============================================================
// Statement formatting
// ============================================================

pub fn format_stmt(s: &Stmt, depth: usize) -> String {
    let pad = indent(depth);
    match s {
        Stmt::Require(e) => format!("{pad}require {}", format_expr_inline(e)),
        Stmt::Let { name, value } => {
            format!("{pad}let {name} = {}", format_expr_inline(value))
        }
        Stmt::LetNewSubject { name } => {
            format!("{pad}let {name} = new_subject()")
        }
        Stmt::Assert(c) => format!("{pad}assert {}", format_claim(c)),
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
fn format_expr(e: &Expr, depth: usize) -> String {
    let pad = indent(depth);
    match e {
        Expr::And(exprs) if exprs.len() > 1 => {
            let mut out = format!("{pad}and(\n");
            for (i, sub) in exprs.iter().enumerate() {
                out.push_str(&format_expr(sub, depth + 1));
                if i + 1 < exprs.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push(')');
            out
        }
        Expr::Implies { left, right } => {
            let mut out = format!("{pad}implies(\n");
            out.push_str(&format_expr(left, depth + 1));
            out.push_str(",\n");
            out.push_str(&format_expr(right, depth + 1));
            out.push('\n');
            out.push_str(&pad);
            out.push(')');
            out
        }
        _ => format!("{pad}{}", format_expr_inline(e)),
    }
}

/// One-line expression rendering. Used inline in `require`, `let`,
/// and as the base case of [`format_expr`] for leaf-shaped nodes.
fn format_expr_inline(e: &Expr) -> String {
    match e {
        Expr::Claim { predicate, args } => format_predicate_call(predicate, args),
        Expr::Implies { left, right } => format!(
            "implies({}, {})",
            format_expr_inline(left),
            format_expr_inline(right)
        ),
        Expr::Exists { binding, body } => {
            format!("exists({binding}, {})", format_expr_inline(body))
        }
        Expr::And(exprs) => {
            let inner: Vec<String> = exprs.iter().map(format_expr_inline).collect();
            format!("and({})", inner.join(", "))
        }
        Expr::Not(inner) => format!("not {}", format_expr_inline(inner)),
        Expr::Neq(t1, t2) => format!("{} != {}", format_term(t1), format_term(t2)),
        Expr::Term(t) => format_term(t),
        Expr::Eq(l, r) => format!(
            "{} == {}",
            format_expr_inline(l),
            format_expr_inline(r)
        ),
        Expr::Le(l, r) => format!(
            "{} <= {}",
            format_expr_inline(l),
            format_expr_inline(r)
        ),
        Expr::DateLe(l, r) => format!(
            "{} <date= {}",
            format_expr_inline(l),
            format_expr_inline(r)
        ),
        Expr::Sub(l, r) => format!(
            "({} - {})",
            format_expr_inline(l),
            format_expr_inline(r)
        ),
        Expr::Add(l, r) => format!(
            "({} + {})",
            format_expr_inline(l),
            format_expr_inline(r)
        ),
        Expr::Sum {
            value,
            binding,
            body,
        } => format!(
            "sum({} | {} in {})",
            format_term(value),
            binding,
            format_expr_inline(body)
        ),
        Expr::Forall {
            binding,
            source,
            body,
        } => format!(
            "forall({} in {}, {})",
            binding,
            format_expr_inline(source),
            format_expr_inline(body)
        ),
        Expr::In(elem, coll) => format!("{} in {}", format_term(elem), format_term(coll)),
        Expr::ValueOf {
            predicate,
            args,
            default,
        } => {
            let base = format!("value_of {}", format_predicate_call(predicate, args));
            match default {
                Some(d) => format!("{base} ?? {}", format_expr_inline(d)),
                None => base,
            }
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
        Term::Actor => "$actor".to_string(),
    }
}

fn format_value(v: &Value) -> String {
    match v {
        // Subjects are quoted to disambiguate from variable names.
        Value::Subject(s) => format!("\"{s}\""),
        Value::Decimal(s) => s.clone(),
        Value::Date(s) => s.clone(),
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
        assert!(s.contains("  assert Trial(trial_id)"));
        assert!(s.contains("  emit TrialOpened(trial_id)"));
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
            forall(
                "w",
                claim("U", vec![var("w")]),
                claim("V", vec![var("w")]),
            ),
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

        // Each variant contributes at least one recognisable token.
        assert!(s.contains("P(x, _)"));
        assert!(s.contains("not Q(x)"));
        assert!(s.contains("implies("));
        assert!(s.contains("exists(z"));
        assert!(s.contains("forall(w"));
        assert!(s.contains("a == b"));
        assert!(s.contains("a != b"));
        assert!(s.contains("a <= b"));
        assert!(s.contains("d1 <date= d2"));
        assert!(s.contains("(p + q)"));
        assert!(s.contains("(p - q)"));
        assert!(s.contains("sum(v |"));
        assert!(s.contains("e in coll"));
        assert!(s.contains("value_of X(k, _)"));
        assert!(s.contains("$actor"));
    }

    #[test]
    fn format_term_renders_literals_subject_decimal_date() {
        assert_eq!(
            format_term(&Term::Literal(Value::Subject("foo".to_string()))),
            "\"foo\""
        );
        assert_eq!(
            format_term(&Term::Literal(Value::Decimal("1250.75".to_string()))),
            "1250.75"
        );
        assert_eq!(
            format_term(&Term::Literal(Value::Date("2026-03-12".to_string()))),
            "2026-03-12"
        );
        assert_eq!(format_term(&Term::Wildcard), "_");
        assert_eq!(format_term(&Term::Actor), "$actor");
        assert_eq!(format_term(&Term::Var("x".to_string())), "x");
    }
}
