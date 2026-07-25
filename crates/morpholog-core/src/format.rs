//! Human-readable pretty-printer for [`Program`]s and their components.
//!
//! The IR derives `Debug`, which is unreadable past a few claims deep.
//! This module renders the same IR in structured indented form for test
//! `panic!` messages, CLI inspection, and kernel diagnostic strings.
//! The output round-trips through `parse_program`.
//!
//! Output style:
//!
//! - **One concept per line where possible.** Statement bodies expand
//!   vertically; sub-expressions inline unless they contain claims or
//!   `And`/`Or`-style branching.
//! - **Result ends with a trailing newline** so callers can append to a
//!   `String` or write to a stream without composing their own
//!   separator. Each per-section helper produces a newline-terminated
//!   block.
//!
//! The exhaustive matches are a deliberate cost: every new IR variant
//! requires a new arm, a compile-time gate that no addition silently
//! degrades the rendering.

use crate::{
    ArithOp, Claim, CompareOp, Definition, DerivedClaim, Discipline, Intent, Invariant,
    InvariantOrigin, OrderedDomain, PredicateDecl, Program, Prop, Stmt, Term, Transformation,
    Value, ValueExpr, Var,
};

/// The surface token for an ordered comparison. The single source of
/// truth for rendering `Prop::Compare` (used by the formatter and the
/// static checker's diagnostics); the parser holds the inverse mapping,
/// and the round-trip test couples the two.
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
        // Instants: "at" is the natural preposition for a point on the
        // timeline, and the strictly_* forms keep the boundary explicit
        // where a dispute would turn on it.
        (OrderedDomain::Timestamp, CompareOp::Le) => "at_or_before",
        (OrderedDomain::Timestamp, CompareOp::Lt) => "strictly_before",
        (OrderedDomain::Timestamp, CompareOp::Ge) => "at_or_after",
        (OrderedDomain::Timestamp, CompareOp::Gt) => "strictly_after",
        // Spans: read as length comparisons - `counted no_longer_than
        // allowed` is the laytime sentence verbatim.
        (OrderedDomain::Duration, CompareOp::Le) => "no_longer_than",
        (OrderedDomain::Duration, CompareOp::Lt) => "shorter_than",
        (OrderedDomain::Duration, CompareOp::Ge) => "no_shorter_than",
        (OrderedDomain::Duration, CompareOp::Gt) => "longer_than",
    }
}

/// The surface token for a binary arithmetic operator. The single source
/// of truth for rendering `ValueExpr::Arith`; the infix operators
/// (`is_infix`) print between their operands, the rest as `token(l, r)`.
/// The parser holds the inverse mapping, and the round-trip test couples
/// the two.
pub(crate) fn arith_token(op: ArithOp) -> &'static str {
    match op {
        ArithOp::Add => "+",
        ArithOp::Sub => "-",
        ArithOp::Mul => "*",
        ArithOp::Div => "/",
        ArithOp::Mod => "%",
        ArithOp::Min => "min",
        ArithOp::Max => "max",
    }
}

/// The canonical content hash of a programme: `sha256:<hex>` over the
/// formatter's canonical rendering. The round-trip property makes that
/// rendering a canonical form, so formatting-only edits and comments do
/// not change the hash - this is rules identity, not file identity. The
/// `sha256:` prefix keeps it self-describing if the algorithm changes.
pub fn canonical_hash(p: &Program) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format_program(p).as_bytes());
    format!("sha256:{digest:x}")
}

/// Top-level entry. Returns a multi-line string terminated by a
/// final `\n`, so callers can write directly to a stream or append
/// to an existing buffer.
pub fn format_program(p: &Program) -> String {
    let mut out = String::new();
    out.push_str(&format!("program {}\n", p.name));

    // Predicates render first as the programme's vocabulary contract,
    // so the reader can interpret every subsequent claim reference. One
    // blank line separates the section; declarations stack consecutively.
    if !p.predicates.is_empty() {
        out.push('\n');
        for decl in &p.predicates {
            out.push_str(&format_predicate_decl(decl));
        }
    }

    // Intents render in their own section after predicates, matching
    // the two-vocabulary distinction visually.
    if !p.intents.is_empty() {
        out.push('\n');
        for decl in &p.intents {
            out.push_str(&format_intent_decl(decl));
        }
    }

    for def in &p.definitions {
        out.push('\n');
        out.push_str(&format_definition(def));
    }

    for inv in &p.invariants {
        // Discipline-generated invariants are implied by the
        // declaration clauses rendered above; printing them too would
        // duplicate them on reparse (lowering regenerates them).
        if inv.origin == InvariantOrigin::Discipline {
            continue;
        }
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
pub(crate) fn format_definition(def: &Definition) -> String {
    let params: Vec<String> = def.parameters.iter().map(ToString::to_string).collect();
    let mut out = String::new();
    out.push_str(&format!("define {}({}):\n", def.name, params.join(", ")));
    out.push_str(&indent(1));
    out.push_str(&format_prop_inline(&def.body));
    out.push('\n');
    out
}

pub(crate) fn format_invariant(inv: &Invariant) -> String {
    let mut out = String::new();
    // Surface has no version syntax in v0; the IR's `version` field
    // defaults to 1 and the formatter omits it.
    out.push_str(&format!("invariant {}:\n", inv.name));
    out.push_str(&indent(1));
    out.push_str(&format_prop_inline(&inv.body));
    out.push('\n');
    out
}

pub(crate) fn format_transformation(t: &Transformation) -> String {
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
        out.push_str(&format_stmt(stmt, 1));
        out.push('\n');
    }
    out
}

pub(crate) fn format_derived_claim(d: &DerivedClaim) -> String {
    // The surface grammar requires at least one `value` clause; an
    // empty `values` Vec would format to text the parser refuses.
    // The kernel doesn't enforce this today, so panic with a clear
    // message rather than silently emit unparseable .morph.
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
    out.push_str(&format!("over {}\n", format_prop_inline(&d.domain)));
    for v in &d.values {
        out.push_str(&indent(1));
        out.push_str(&format!(
            "value {} = {}\n",
            v.name,
            format_value_inline(&v.expr)
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
        Stmt::Require(p) => format!("{pad}require {}", format_prop_inline(p)),
        Stmt::BindOne(p) => {
            // The surface grammar restricts `bind` to a claim pattern,
            // though the IR's `Stmt::BindOne(Prop)` is broader. Panic on
            // non-Claim shapes rather than emit text the parser refuses.
            assert!(
                matches!(p, Prop::Claim { .. }),
                "format_stmt: bind requires a claim pattern; got {p:?}",
            );
            format!("{pad}bind {}", format_prop_inline(p))
        }
        Stmt::Let { name, value } => {
            format!("{pad}let {name} = {}", format_value_inline(value))
        }
        Stmt::LetNewSubject { name } => {
            format!("{pad}let {name} = new Subject()")
        }
        Stmt::Assert(c) => format!("{pad}admit {}", format_claim(c)),
        Stmt::Retract { predicate, args } => {
            format!(
                "{pad}retract {}",
                format_predicate_call(predicate.as_str(), args)
            )
        }
        Stmt::Emit(i) => format!("{pad}emit {}", format_intent(i)),
        Stmt::For {
            binding,
            collection,
            body,
        } => {
            let mut out = format!(
                "{pad}for {binding} in {}:\n",
                format_value_inline(collection)
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

/// One-line rendering of a [`Prop`], the proposition printer in the
/// kernel. Used in `require`/`bind`, invariant bodies, derived-claim
/// domains, and kernel diagnostic paths (rejection reasons, multi-match
/// errors). Its value-operand renderer is [`format_value_inline`]; the
/// two compose because the sorts are mutually recursive.
pub fn format_prop_inline(p: &Prop) -> String {
    // Composite sub-propositions are wrapped in parens unconditionally;
    // verbose but unambiguous and round-trips through `parse_expression`
    // (or `parse_program` once embedded in a programme body). The surface
    // comparator precedence (arithmetic > comparators > not > and >
    // implies) makes the parens a no-op for the parser.
    fn prop_primary(p: &Prop) -> String {
        match p {
            Prop::Claim { predicate, args } => format_predicate_call(predicate.as_str(), args),
            Prop::Defined { name, args } => format_predicate_call(name.as_str(), args),
            // `pre(...)` is function-call-shape; no outer parens needed.
            Prop::Pre(inner) => format!("pre({})", format_prop_inline(inner)),
            _ => format!("({})", format_prop_inline(p)),
        }
    }

    match p {
        Prop::Claim { predicate, args } => format_predicate_call(predicate.as_str(), args),
        // A definition call renders exactly like a claim reference; the
        // parser re-resolves it by name, so round-trip holds.
        Prop::Defined { name, args } => format_predicate_call(name.as_str(), args),

        // Comparators relate two value expressions.
        Prop::Eq(l, r) => format!("{} = {}", value_primary(l), value_primary(r)),
        Prop::Compare {
            op,
            domain,
            left,
            right,
        } => format!(
            "{} {} {}",
            value_primary(left),
            compare_token(*op, *domain),
            value_primary(right)
        ),
        Prop::Neq(lhs, rhs) => format!("{} != {}", value_primary(lhs), value_primary(rhs)),
        Prop::In(elem, coll) => format!("{} in {}", format_term(elem), format_term(coll)),

        // Boolean composition: prefix `not`, infix `and`/`or`/`implies`.
        Prop::Pre(inner) => format!("pre({})", format_prop_inline(inner)),
        Prop::Not(inner) => format!("not {}", prop_primary(inner)),
        Prop::And(props) => {
            let inner: Vec<String> = props.iter().map(prop_primary).collect();
            inner.join(" and ")
        }
        Prop::Or(props) => {
            let inner: Vec<String> = props.iter().map(prop_primary).collect();
            inner.join(" or ")
        }
        Prop::Xor(left, right) => {
            format!("{} xor {}", prop_primary(left), prop_primary(right))
        }
        Prop::Implies { left, right } => {
            format!("{} implies {}", prop_primary(left), prop_primary(right))
        }

        // Quantifiers: colon-block form. Source for `forall` is a
        // primary proposition (typically a Claim or a lifted `In`).
        Prop::Exists { binding, body } => {
            format!("exists {binding}: {}", format_prop_inline(body))
        }
        Prop::Forall {
            binding,
            source,
            body,
        } => {
            // The IR's source is a Prop; the natural surface form
            // `forall x in coll:` is built by the parser as
            // `Prop::In(Term::Var(x), coll)`. Detect that lifted shape
            // and emit the natural surface; otherwise fall back to
            // whatever primary proposition the source is.
            let source_text = match source.as_ref() {
                Prop::In(Term::Var(b), coll) if b == binding => format_term(coll),
                _ => prop_primary(source),
            };
            format!(
                "forall {binding} in {source_text}: {}",
                format_prop_inline(body)
            )
        }
    }
}

/// Render a value expression for an operand position, wrapping a
/// composite arithmetic subtree in parens so the surface text reparses
/// to the same tree. `Term`, `Sum`, and `ValueOf` are already primary-
/// shaped; `Add`/`Sub` are parenthesised.
fn value_primary(e: &ValueExpr) -> String {
    match e {
        ValueExpr::Term(t) => format_term(t),
        // Infix arithmetic is the only ambiguous form: parenthesise it so
        // the surface text reparses to the same tree. Everything else is
        // self-delimiting (a keyword or function with its own parens).
        ValueExpr::Arith { op, .. } if op.is_infix() => {
            format!("({})", format_value_inline(e))
        }
        _ => format_value_inline(e),
    }
}

/// One-line rendering of a [`ValueExpr`], the value-expression printer
/// in the kernel. Used in `let`/`for` collections and derived-claim
/// value expressions. Its proposition renderer (for a `sum` body) is
/// [`format_prop_inline`].
pub fn format_value_inline(e: &ValueExpr) -> String {
    match e {
        ValueExpr::Term(t) => format_term(t),
        ValueExpr::Sum {
            value,
            body,
            seed: _,
        } => {
            format!("sum({} | {})", format_term(value), format_prop_inline(body))
        }
        ValueExpr::ValueOf {
            predicate,
            args,
            default,
        } => {
            let base = format!("value {}", format_predicate_call(predicate.as_str(), args));
            match default {
                Some(d) => format!("{base} default {}", format_value_inline(d)),
                None => base,
            }
        }
        ValueExpr::Arith { op, left, right } => {
            let token = arith_token(*op);
            if op.is_infix() {
                format!("{} {token} {}", value_primary(left), value_primary(right))
            } else {
                format!(
                    "{token}({}, {})",
                    format_value_inline(left),
                    format_value_inline(right)
                )
            }
        }
        ValueExpr::Abs(operand) => format!("abs({})", format_value_inline(operand)),
        ValueExpr::Round { value, quantum } => format!(
            "round({}, {})",
            format_value_inline(value),
            format_value_inline(quantum)
        ),
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
    format_predicate_call(c.predicate.as_str(), &c.args)
}

fn format_intent(i: &Intent) -> String {
    format_predicate_call(i.name.as_str(), &i.args)
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
        // Subjects use the `#name` sigil; the surface lexer accepts
        // only an ASCII identifier after `#`. Panic on any subject that
        // wouldn't round-trip, so the issue surfaces at format time
        // rather than as a confusing downstream parse failure.
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
        // Durations use an explicit constructor form rather than a
        // bare-literal DSL: boring on purpose. No quotes - the payload
        // is identifier-shaped, and the surface has no string literals.
        Value::Duration(s) => format!("duration({s})"),
        // Quantity literals are amount-then-unit juxtaposition: the
        // way a charterparty or an invoice writes them.
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
    //! Tests pin the *shape* of the rendered output - specific tokens
    //! it must contain - not byte-for-byte equality, except where a
    //! test explicitly checks exact bytes. What must not change is that
    //! every variant is reachable and produces readable output.

    use super::*;
    use crate::ir_builder::*;
    use crate::{PredicateArgKind, Value};

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

    /// Pins the predicate section layout in `format_program`: one blank
    /// line separates the section from the header, then declarations
    /// stack consecutively with no intervening blank lines.
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

    /// Every `PredicateArgKind` variant has a stable display name via
    /// the `Display` impl the formatter and the validation errors
    /// share - the declaration syntax IS the diagnostic syntax, so the
    /// unit always renders (`Decimal[USD]`) and the surfaces cannot
    /// drift. The exhaustive impl means a future variant must extend it.
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
        // One proposition exercising every Prop variant; each must
        // produce a recognisable token. A new variant without a printer
        // arm fails to compile against the exhaustive match. Comparator
        // operands are value expressions, so this also reaches the value
        // renderer for the bare-term and arithmetic-operand cases.
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
        // One value expression exercising every ValueExpr variant; each
        // must produce a recognisable token. A new variant without a
        // printer arm fails to compile against the exhaustive match.
        let e = add(
            sub(term(var("p")), term(var("q"))),
            sum(var("v"), claim("W", vec![var("v")])),
        );
        let s = format_value_inline(&e);
        assert!(s.contains("p - q"));
        assert!(s.contains("sum(v |"));

        let vo = value_of("X", vec![var("k"), wildcard()]);
        assert!(format_value_inline(&vo).contains("value X(k, _)"));

        // Mul, Div, Min, Max - including the nested collar shape
        // `min(_, max(0, _))`; min/max are self-delimiting, so their
        // operands render without extra parens.
        let collar = min(
            mul(term(var("a")), term(var("b"))),
            max(term(dec("0")), div(term(var("c")), term(var("d")))),
        );
        let printed = format_value_inline(&collar);
        assert_eq!(printed, "min(a * b, max(0, c / d))");

        // Mod renders infix `%` and parenthesises inside another operand,
        // like the other infix arithmetic: the chess parity shape.
        let parity = modulo(add(term(var("f")), term(var("r"))), term(dec("2")));
        assert_eq!(format_value_inline(&parity), "(f + r) % 2");
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

    /// `format_program` documents that its output ends with a
    /// trailing newline. Pin that contract so callers can rely on it
    /// (write directly to a stream, append without composing a
    /// separator).
    #[test]
    fn format_program_output_ends_with_newline() {
        let p = program("demo")
            .transformations(vec![transformation("noop", vec![], vec![])])
            .build();
        let s = format_program(&p);
        assert!(s.ends_with('\n'), "expected trailing newline; got: {s:?}");
    }
}
