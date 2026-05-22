//! Tests for the layout normalisation pass.
//!
//! These exercise the pass in isolation - independent of any
//! parser productions - so layout bugs surface here, not as
//! confusing parse errors. The contract being tested is the
//! pass's input/output: `(source, lex output) -> token stream
//! enriched with Indent/Dedent` (or diagnostics).
//!
//! `apply_layout` is `pub` (so integration tests can call it
//! directly); the crate's `parse_program` entry point applies
//! layout transparently. The public surface is small - just
//! `apply_layout` and the `Token` enum it reads from - and is
//! kept that way to make future tooling work (formatter,
//! source-mapper) easier to slot in.

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
    // One indented transformation body; one Indent at the body
    // start, one Dedent at EOF.
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
    // An indented quantifier body inside an indented invariant.
    // Two indent levels (col 4 for invariant body, col 8 for the
    // quantifier body), so two Indents and (at EOF) two Dedents.
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
    // Two statements at the same indentation inside one block
    // should produce one Indent at the start and one Dedent at
    // EOF; no Indent/Dedent between the statements.
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
    // The body of sum(...) spans two lines but is inside parens;
    // no Indent / Dedent should be emitted for the continuation.
    let toks = tokens(
        "program demo\n\
         invariant cap:\n\
         \x20\x20\x20\x20sum(amount\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20| Foo(amount))\n",
    );
    let indents = toks.iter().filter(|t| **t == Token::Indent).count();
    let dedents = toks.iter().filter(|t| **t == Token::Dedent).count();
    // One Indent for the invariant body; one Dedent at EOF;
    // the continuation inside sum(...) parens does NOT produce
    // a deeper Indent/Dedent.
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
    // Indent to column 4, then dedent to column 2 - which isn't
    // on the indent stack ([0, 4]). Diagnostic.
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
    // Three nested blocks open, no trailing newline; EOF should
    // emit a Dedent for each.
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
    // Top-level declarations must start at column 0. Source that
    // begins with leading spaces should produce a diagnostic, not
    // silently parse as if there were no indentation.
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
    // The lexer strips comments, so a `\t` in a comment-only line's
    // indentation would be invisible to the per-token indent check.
    // The whole-gap tab scan catches it.
    let source = "program demo\n\
                  transformation foo():\n\
                  \trequire A()\n";
    // Note: the `\t` here is on the line that becomes the first
    // statement of the body - already caught by the indent_text
    // check. Use a separate case to specifically exercise the
    // comment-line tab scan:
    let with_comment_tab = "program demo\n\
                            -- ok comment\n\
                            \t-- tab-indented comment\n\
                            predicate Foo(x: Subject)\n";
    // The lexer strips both comment lines. The gap between the
    // `\n` after `demo` and the first token of `predicate` contains
    // a tab in indentation of a comment-only line. The full-gap
    // scan should diagnose it.
    let errs = tokens_or_err(with_comment_tab);
    // We expect a tab diagnostic; if the scan misses, this test
    // would silently pass with `Ok(...)`. Be explicit.
    let _ = source; // keep referenced
    let errs = errs.expect_err("tab in comment-line indent should fail");
    assert!(
        errs.iter().any(|m| m.contains("tab")),
        "expected tab diagnostic; got: {errs:?}"
    );
}

/// Indentation that's "spaces then tab" (e.g. `"  \t-- comment"`)
/// must also be rejected, not just a leading-tab line. The lexer
/// strips comments, so the layout pass scans the raw gap for any
/// tab in the leading-whitespace run of any line.
#[test]
fn space_then_tab_indentation_on_comment_line_diagnosed() {
    let source = "program demo\n  \t-- a tab-indented comment\npredicate Foo(x: Subject)\n";
    let errs = tokens_or_err(source).expect_err("space+tab in comment indent should fail");
    assert!(
        errs.iter().any(|m| m.contains("tab")),
        "expected tab diagnostic; got: {errs:?}"
    );
}
