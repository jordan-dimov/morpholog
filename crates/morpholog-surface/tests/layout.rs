//! The layout pass on its own, apart from the parser, so layout bugs show up here rather than as
//! confusing parse errors. Each test checks the tokens with Indent/Dedent inserted, or the
//! diagnostics.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_surface::layout::apply_layout;
use morpholog_surface::lexer::{Token, lex};

fn tokens(source: &str) -> Vec<Token> {
    let lexed = lex(source).expect("lex failure in test fixture");
    let laid_out = apply_layout(source, lexed).expect("layout failure in test fixture");
    laid_out.into_iter().map(|(t, _)| t).collect()
}

fn tokens_or_err(source: &str) -> Result<Vec<Token>, Vec<String>> {
    let lexed = lex(source).expect("lex failure in test fixture");
    match apply_layout(source, lexed) {
        Ok(laid_out) => Ok(laid_out.into_iter().map(|(t, _)| t).collect()),
        Err(diags) => Err(diags.into_iter().map(|d| d.message).collect()),
    }
}

// ---- Flat input: no Indent / Dedent ----

#[test]
fn single_line_no_layout_tokens() {
    let toks = tokens("program demo");
    assert!(
        !toks.contains(&Token::Indent) && !toks.contains(&Token::Dedent),
        "single line produced layout tokens: {toks:?}"
    );
}

#[test]
fn multiple_top_level_decls_no_layout() {
    // All at column 0; no Indent/Dedent needed.
    let toks = tokens(
        "program demo\n\
         predicate Foo(a: Subject)\n\
         predicate Bar(b: Decimal)\n",
    );
    assert!(
        !toks.contains(&Token::Indent),
        "no Indent expected at column 0; got {toks:?}"
    );
    assert!(
        !toks.contains(&Token::Dedent),
        "no Dedent expected at column 0; got {toks:?}"
    );
}

// ---- Simple indented block ----

#[test]
fn invariant_body_indented_emits_one_indent_one_dedent() {
    let toks = tokens(
        "program demo\n\
         invariant x:\n\
         \x20\x20\x20\x20Foo(y)\n",
    );
    let indents = toks.iter().filter(|t| **t == Token::Indent).count();
    let dedents = toks.iter().filter(|t| **t == Token::Dedent).count();
    assert_eq!(
        indents, 1,
        "expected exactly one Indent; got tokens: {toks:?}"
    );
    assert_eq!(
        dedents, 1,
        "expected exactly one Dedent at EOF; got: {toks:?}"
    );
}

#[test]
fn single_indented_block_emits_one_indent_one_dedent() {
    // One Indent at the body start, one Dedent at EOF.
    let toks = tokens(
        "program demo\n\
         transformation foo(x):\n\
         \x20\x20\x20\x20require A(x)\n\
         \x20\x20\x20\x20bind B(x, y)\n",
    );
    let indents = toks.iter().filter(|t| **t == Token::Indent).count();
    let dedents = toks.iter().filter(|t| **t == Token::Dedent).count();
    assert_eq!(
        indents, 1,
        "one Indent for the transformation body; got {toks:?}"
    );
    assert_eq!(dedents, 1, "one Dedent at EOF; got {toks:?}");
}

#[test]
fn genuinely_nested_indentation_emits_two_indents_two_dedents() {
    // A quantifier body (col 8) inside an invariant body (col 4): two Indents, two Dedents.
    let toks = tokens(
        "program demo\n\
         invariant cap:\n\
         \x20\x20\x20\x20forall x in xs:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Foo(x)\n",
    );
    let indents = toks.iter().filter(|t| **t == Token::Indent).count();
    let dedents = toks.iter().filter(|t| **t == Token::Dedent).count();
    assert_eq!(indents, 2, "two Indents for nested layout; got {toks:?}");
    assert_eq!(dedents, 2, "two matching Dedents at EOF; got {toks:?}");
}

#[test]
fn dedent_returns_to_outer_level() {
    // Two transformations side-by-side: indent into the first,
    // dedent back to column 0, indent into the second, dedent at EOF.
    let toks = tokens(
        "program demo\n\
         transformation a():\n\
         \x20\x20\x20\x20require X()\n\
         transformation b():\n\
         \x20\x20\x20\x20require Y()\n",
    );
    let indents = toks.iter().filter(|t| **t == Token::Indent).count();
    let dedents = toks.iter().filter(|t| **t == Token::Dedent).count();
    assert_eq!(
        indents, 2,
        "one Indent per transformation body; got {toks:?}"
    );
    assert_eq!(dedents, 2, "matching Dedents; got {toks:?}");
}

// ---- Same-line / same-column behaviour ----

#[test]
fn same_indent_continues_block_without_extra_tokens() {
    // Nothing between two statements at the same indentation.
    let toks = tokens(
        "program demo\n\
         transformation foo():\n\
         \x20\x20\x20\x20require A()\n\
         \x20\x20\x20\x20require B()\n",
    );
    let indents = toks.iter().filter(|t| **t == Token::Indent).count();
    let dedents = toks.iter().filter(|t| **t == Token::Dedent).count();
    assert_eq!(
        indents, 1,
        "same-level statements share one block; got {toks:?}"
    );
    assert_eq!(
        dedents, 1,
        "same-level statements share one block; got {toks:?}"
    );
}

// ---- Blank lines and comments ----

#[test]
fn blank_lines_inside_block_do_not_break_layout() {
    let toks = tokens(
        "program demo\n\
         transformation foo():\n\
         \x20\x20\x20\x20require A()\n\
         \n\
         \x20\x20\x20\x20require B()\n",
    );
    let indents = toks.iter().filter(|t| **t == Token::Indent).count();
    let dedents = toks.iter().filter(|t| **t == Token::Dedent).count();
    assert_eq!(
        indents, 1,
        "blank line should not break the block; got {toks:?}"
    );
    assert_eq!(dedents, 1);
}

#[test]
fn comment_only_lines_inside_block_do_not_break_layout() {
    let toks = tokens(
        "program demo\n\
         transformation foo():\n\
         \x20\x20\x20\x20require A()\n\
         \x20\x20\x20\x20-- a comment\n\
         \x20\x20\x20\x20require B()\n",
    );
    let indents = toks.iter().filter(|t| **t == Token::Indent).count();
    let dedents = toks.iter().filter(|t| **t == Token::Dedent).count();
    assert_eq!(indents, 1);
    assert_eq!(dedents, 1);
}

// ---- Paren-protected line continuation ----

#[test]
fn parenthesised_expression_spans_lines_without_layout() {
    // The sum(...) body spans two lines inside parens, so no layout tokens.
    let toks = tokens(
        "program demo\n\
         invariant cap:\n\
         \x20\x20\x20\x20sum(amount\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20| Foo(amount))\n",
    );
    let indents = toks.iter().filter(|t| **t == Token::Indent).count();
    let dedents = toks.iter().filter(|t| **t == Token::Dedent).count();
    // Only the invariant body's Indent and Dedent.
    assert_eq!(
        indents, 1,
        "parenthesised continuation should not deepen layout; got {toks:?}"
    );
    assert_eq!(dedents, 1);
}

// ---- Error cases ----

#[test]
fn tab_indentation_is_rejected() {
    let errs = tokens_or_err(
        "program demo\n\
         transformation foo():\n\
         \trequire A()\n",
    )
    .expect_err("tab indentation should fail");
    assert!(
        errs.iter().any(|m| m.contains("tab")),
        "expected a tab-indentation diagnostic; got: {errs:?}"
    );
}

#[test]
fn misaligned_dedent_is_rejected() {
    // Column 2 is not an open level ([0, 4]).
    let errs = tokens_or_err(
        "program demo\n\
         transformation foo():\n\
         \x20\x20\x20\x20require A()\n\
         \x20\x20require B()\n",
    )
    .expect_err("misaligned dedent should fail");
    assert!(
        errs.iter()
            .any(|m| m.contains("indentation does not match")),
        "expected a misaligned-dedent diagnostic; got: {errs:?}"
    );
}

// ---- EOF behaviour ----

#[test]
fn eof_closes_all_open_blocks() {
    // Three blocks open at EOF with no trailing newline: one Dedent each.
    let toks = tokens(
        "program demo\n\
         transformation foo():\n\
         \x20\x20\x20\x20require A()",
    );
    let dedents = toks.iter().filter(|t| **t == Token::Dedent).count();
    assert_eq!(
        dedents, 1,
        "one outermost block to close at EOF; got {toks:?}"
    );
}

#[test]
fn empty_input_returns_empty_stream() {
    let toks = tokens("");
    assert!(toks.is_empty(), "empty input should produce no tokens");
}

#[test]
fn whitespace_only_input_returns_empty_stream() {
    let toks = tokens("   \n  \n");
    assert!(
        toks.is_empty(),
        "whitespace-only input should produce no tokens; got {toks:?}"
    );
}

// ---- Leading indentation diagnostics ----

#[test]
fn leading_spaces_before_first_token_diagnosed() {
    // Top-level declarations start at column 0; leading spaces are an error, not ignored.
    let errs = tokens_or_err("    program demo\n").expect_err("leading spaces should fail");
    assert!(
        errs.iter().any(|m| m.contains("leading indentation")),
        "expected leading-indent diagnostic; got: {errs:?}"
    );
}

#[test]
fn leading_tab_before_first_token_diagnosed() {
    let errs = tokens_or_err("\tprogram demo\n").expect_err("leading tab should fail");
    assert!(
        errs.iter().any(|m| m.contains("tab")),
        "expected tab diagnostic; got: {errs:?}"
    );
}

#[test]
fn tab_on_comment_only_line_is_diagnosed() {
    // A comment-only line produces no token, so its indentation needs its own tab check.
    let source = "program demo\n\
                  transformation foo():\n\
                  \trequire A()\n";
    // That tab is on a statement line, caught by the ordinary check. This one is on a comment:
    let with_comment_tab = "program demo\n\
                            -- ok comment\n\
                            \t-- tab-indented comment\n\
                            predicate Foo(x: Subject)\n";
    let errs = tokens_or_err(with_comment_tab);
    let _ = source; // keep referenced
    let errs = errs.expect_err("tab in comment-line indent should fail");
    assert!(
        errs.iter().any(|m| m.contains("tab")),
        "expected tab diagnostic; got: {errs:?}"
    );
}

/// A tab after spaces (`"  \t-- comment"`) is rejected too, not only a leading tab.
#[test]
fn space_then_tab_indentation_on_comment_line_diagnosed() {
    let source = "program demo\n  \t-- a tab-indented comment\npredicate Foo(x: Subject)\n";
    let errs = tokens_or_err(source).expect_err("space+tab in comment indent should fail");
    assert!(
        errs.iter().any(|m| m.contains("tab")),
        "expected tab diagnostic; got: {errs:?}"
    );
}
