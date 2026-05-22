//! Layout normalisation pass.
//!
//! Sits between the character-level lexer and the structural
//! parser. Consumes the lexer's `(Token, Span)` stream + the
//! original source string, and produces an enriched stream with
//! virtual [`Token::Indent`] and [`Token::Dedent`] tokens at block
//! boundaries.
//!
//! The layout pass exists because Morpholog's surface uses
//! indentation for block structure (per the doctrine in
//! `docs/scope-and-ambition.md`). Splitting it out keeps the
//! character-level lexer ignorant of line structure and lets the
//! structural parser match `Indent`/`Dedent` as ordinary tokens.
//!
//! Design choices:
//!
//! - **No `Newline` tokens emitted.** Each statement starts with
//!   its own keyword (`require`, `bind`, `let`, `admit`, etc.) and
//!   each top-level declaration starts with its own keyword
//!   (`program`, `predicate`, `invariant`, `transformation`). The
//!   keyword anchors statement boundaries; no separator token is
//!   needed. Bracketed expressions can therefore span lines without
//!   any layout interaction.
//! - **Spaces only.** Tab characters in indentation emit a
//!   diagnostic. Mixed tabs/spaces are a known source of ambiguity
//!   in indentation-sensitive languages; the floor is to refuse the
//!   ambiguity rather than make a guess.
//! - **Parens disable layout.** When a token's preceding gap is
//!   inside open parentheses (paren depth > 0), the gap's newlines
//!   do not trigger Indent / Dedent. This lets long expressions
//!   span lines without being mistaken for block boundaries:
//!   ```text
//!   require sum(amount | SettlementPaid(claim, amount))
//!       + proposed
//!       <= limit
//!   ```
//! - **Comments and blank lines do not affect indentation.**
//!   Comments are stripped at the lexer, so the gap between two
//!   tokens may contain multiple newlines and comments; only the
//!   indentation of the NEXT real token matters.

use crate::diagnostics::Diagnostic;
use crate::lexer::{SpannedToken, Token};

/// Run the layout pass over the given token stream.
///
/// Returns the enriched stream on success, or a list of diagnostics
/// describing layout violations (tab indentation, mis-aligned dedent).
///
/// The pass is non-destructive of the original tokens: all of them
/// are preserved in order; only Indent / Dedent are inserted.
pub fn apply_layout(
    source: &str,
    tokens: Vec<SpannedToken>,
) -> Result<Vec<SpannedToken>, Vec<Diagnostic>> {
    if tokens.is_empty() {
        return Ok(tokens);
    }

    let mut out: Vec<SpannedToken> = Vec::with_capacity(tokens.len() + 8);
    let mut indent_stack: Vec<usize> = vec![0];
    let mut paren_depth: i64 = 0;
    let mut diagnostics: Vec<Diagnostic> = Vec::new();

    // `prev_end` tracks the byte offset just past the previous
    // physical token. The gap `source[prev_end..token.span.start]`
    // is whatever was between them: whitespace, newlines, and any
    // comments the lexer has already stripped from token output.
    let mut prev_end: usize = 0;

    for (i, (token, span)) in tokens.iter().enumerate() {
        let gap = &source[prev_end..span.start];

        if paren_depth == 0
            && let Some(last_nl_offset) = gap.rfind('\n')
        {
            // Line boundary outside any open parens. Compute the
            // column of this token (1-based on the line; we work in
            // byte counts here, which is fine because tabs are
            // rejected and identifier bytes are ASCII).
            let line_start_in_source = prev_end + last_nl_offset + 1;
            let indent_text = &source[line_start_in_source..span.start];

            if indent_text.contains('\t') {
                diagnostics.push(Diagnostic::error(
                    "tab characters are not allowed in indentation; use spaces",
                    line_start_in_source..span.start,
                ));
                // Continue with what we have; the parser will see the
                // tokens but layout depths may be off. The diagnostic
                // is what we need to surface; the user must fix it.
            }

            // Count non-tab indentation. For now this is bytes; once
            // any non-ASCII identifiers are allowed it stays correct
            // because the leading run is whitespace only.
            let new_indent = indent_text.len();

            // `indent_stack` always has at least one element (the
            // initial `0`); we never pop below it. `unwrap_or(&0)`
            // both expresses that invariant and silences clippy's
            // expect-used lint.
            let current = *indent_stack.last().unwrap_or(&0);
            if new_indent > current {
                indent_stack.push(new_indent);
                out.push((Token::Indent, span.start..span.start));
            } else if new_indent < current {
                while indent_stack.last().copied().unwrap_or(0) > new_indent {
                    indent_stack.pop();
                    out.push((Token::Dedent, span.start..span.start));
                }
                if indent_stack.last().copied().unwrap_or(0) != new_indent {
                    diagnostics.push(Diagnostic::error(
                        format!(
                            "indentation does not match any enclosing block (got {} columns; valid levels are {:?})",
                            new_indent, indent_stack
                        ),
                        line_start_in_source..span.start,
                    ));
                }
            }
            // new_indent == current: continuing the same block; no
            // Indent / Dedent needed.
        } else if i == 0 && span.start > 0 {
            // First token of the file is not at column 0 and there's
            // no preceding newline. That's unexpected indentation
            // (or BOM-like leading content). Check whether the gap
            // is blank-line-only or just leading indent of the first
            // line.
            if !gap.contains('\n') && !gap.chars().all(|c| c.is_whitespace()) {
                // Shouldn't happen given the lexer skips whitespace,
                // but kept as a guard against future lexer changes.
                diagnostics.push(Diagnostic::error(
                    "unexpected leading content before first token",
                    0..span.start,
                ));
            }
        }

        // Track paren depth through this token.
        match token {
            Token::LParen => paren_depth += 1,
            Token::RParen => paren_depth = (paren_depth - 1).max(0),
            _ => {}
        }

        out.push((token.clone(), span.clone()));
        prev_end = span.end;
    }

    // At EOF, close any blocks that the source left open. Position
    // the dedent at the end-of-input so diagnostics that anchor on
    // it land somewhere sensible.
    while indent_stack.len() > 1 {
        indent_stack.pop();
        let eof = source.len();
        out.push((Token::Dedent, eof..eof));
    }

    if diagnostics.is_empty() {
        Ok(out)
    } else {
        Err(diagnostics)
    }
}
