//! Parser for the v0 surface fragment.
//!
//! Grammar (BNF):
//!
//! ```text
//! program        ::= program_header top_level_decl*
//! top_level_decl ::= predicate_decl | invariant_decl
//! program_header ::= "program" Ident
//! predicate_decl ::= "predicate" Ident "(" arg_list? ")"
//! arg_list       ::= arg ("," arg)* ","?
//! arg            ::= Ident ":" Kind
//! Kind           ::= "Subject" | "Decimal" | "Date" | "Bool" | "Collection" | "Any"
//! invariant_decl ::= "invariant" Ident ":" expression
//!
//! expression     ::= quantifier | implies
//! quantifier     ::= "exists" Ident ":" expression
//!                  | "forall" Ident "in" forall_source ":" expression
//! forall_source  ::= "(" expression ")"
//!                  | Ident "(" term_list ")"           -- claim call
//!                  | Ident                             -- variable (auto-lifts to In)
//! implies        ::= and ("implies" implies)?
//! and            ::= not_expr ("and" not_expr)*
//! not_expr       ::= "not" not_expr | comparison
//! comparison     ::= arith (cmp_op arith)?
//! cmp_op         ::= "=" | "!=" | "<=" | "in"
//! arith          ::= primary (("+" | "-") primary)*
//! primary        ::= "(" expression ")"
//!                  | sum_expr | value_expr
//!                  | DecimalLit | DateLit | SubjectLit
//!                  | "_"
//!                  | Ident "(" term_list ")"           -- claim call (args optional)
//!                  | Ident                             -- variable | actor
//! sum_expr       ::= "sum" "(" Ident "|" expression ")"
//! value_expr     ::= "value" Ident "(" term_list ")" ("default" expression)?
//! term_list      ::= (term ("," term)* ","?)?         -- zero or more terms
//! term           ::= Ident | "_" | DecimalLit | DateLit | SubjectLit
//! ```
//!
//! Newlines are insignificant. Trailing commas in argument lists
//! are allowed. Comments are stripped at the lexer; the parser
//! never sees them.
//!
//! Asymmetry to honour: `Expr::In(Term, Term)` operates on *terms*,
//! not expressions, as do claim-call arguments. `Eq`, `Neq`, the
//! comparators, `Sub`, and `Add` operate on full expressions. The
//! parser must therefore reject `a + 1 in xs` (membership is
//! term-only) and `Foo(x + 1, y)` (claim arguments are terms), while
//! `Foo + 1 != Bar` is accepted - `!=` is symmetric with `=`. These
//! constraints follow directly from the IR shape under the
//! `docs/scope-and-ambition.md` surface doctrine.
//!
//! Two disambiguation rules govern the bounded forms:
//!
//! - The `in` keyword is structural inside `forall <ident> in
//!   <source>:` (consumed by the forall production before
//!   reaching comparator-level grammar) and a membership
//!   comparator everywhere else. Positional disambiguation; no
//!   context-sensitive parsing.
//! - `forall x in source: body` accepts the source in a restricted
//!   grammar (bare variable, claim call, or parenthesised
//!   expression) - per the doctrine, value-shaped primaries
//!   (literals, wildcards, `sum`, `value`) cannot be `forall`
//!   sources. When the source is a bare Term-wrapper, the parser
//!   auto-lifts it to `Expr::In(Var(binding), source_term)`.
//!
//! Module layout: this parser file is split by concern. The
//! programme-level parser (`parse_program`, `program_parser`,
//! `RawProgram`, `TopLevelDecl`) lives in [`program`]; the
//! expression-level parser (`parse_expression`, `expression_parser`,
//! `CmpOp`, `expr_as_term`) lives in [`expr`]. The programme
//! parser uses the expression parser for invariant bodies via
//! the `pub(super)` re-export from `expr`.
//!
//! Error recovery: humble. On a parse failure inside a top-level
//! declaration, the parser skips forward to the next `predicate`
//! or `invariant` keyword (or EOF) and continues. The intent is
//! "tell the author about every malformed declaration in one
//! parse run", not a full-language error-recovery framework.
//! Expression parsing does not yet add recovery shapes; a
//! malformed expression surfaces as one diagnostic at the failure
//! site.

mod expr;
mod program;
mod stmt;

pub use expr::parse_expression;
pub use program::parse_program;
