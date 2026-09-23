//! Layout pass: turns indentation into tokens.
//!
//! Runs between the lexer and the parser, inserting [`Token::Indent`] and [`Token::Dedent`] at
//! block boundaries so the lexer stays ignorant of lines and the parser sees blocks as tokens.
//!
//! - **No `Newline` tokens.** Every statement and declaration starts with a keyword, which marks
//!   the boundary.
//! - **Spaces only.** A tab in indentation is an error, rather than a guess at its width.
//! - **Parentheses switch layout off.** Inside open parens, line breaks never open or close a
//!   block, so a long expression can span lines:
//!   ```text
//!   require sum(amount | SettlementPaid(claim, amount))
//!       + proposed
//!       <= limit
//!   ```
//! - **Comments and blank lines do not count.** Only the indentation of the next real token
//!   matters.

use crate::diagnostics::Diagnostic;
use crate::lexer::{SpannedToken, Token};

/// Run the layout pass over the given token stream.
///
/// Returns every input token in order with `Indent` / `Dedent` inserted, or diagnostics for
/// layout errors such as tab indentation or a dedent to no enclosing level.
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

    // End of the previous token. The gap up to the next one holds whitespace and comments.
    let mut prev_end: usize = 0;

    for (i, (token, span)) in tokens.iter().enumerate() {
        let gap = &source[prev_end..span.start];

        if paren_depth == 0
            && let Some(last_nl_offset) = gap.rfind('\n')
        {
            // A new line outside parens. Columns are bytes: indentation is spaces only.
            let line_start_in_source = prev_end + last_nl_offset + 1;
            let indent_text = &source[line_start_in_source..span.start];

            let mut tab_diagnosed_for_this_gap = false;
            if indent_text.contains('\t') {
                diagnostics.push(Diagnostic::error(
                    "tab characters are not allowed in indentation; use spaces",
                    line_start_in_source..span.start,
                ));
                tab_diagnosed_for_this_gap = true;
                // Carry on so later errors still surface; depths may be off.
            }
            // Blank and comment-only lines in the gap produce no tokens, so check their
            // indentation for tabs too. One diagnostic per gap is enough.
            if !tab_diagnosed_for_this_gap {
                for line in gap.split('\n').skip(1) {
                    let indent_end = line
                        .char_indices()
                        .find(|(_, c)| !c.is_whitespace())
                        .map(|(i, _)| i)
                        .unwrap_or(line.len());
                    let indent_run = &line[..indent_end];
                    if indent_run.contains('\t') {
                        diagnostics.push(Diagnostic::error(
                            "tab characters are not allowed in indentation; use spaces",
                            prev_end..span.start,
                        ));
                        break;
                    }
                }
            }

            let new_indent = indent_text.len();

            // The stack never empties below its initial `0`.
            let current = *indent_stack.last().unwrap_or(&0);
            if new_indent > current {
                indent_stack.push(new_indent);
                out.push((Token::Indent, span.start..span.start));
            } else if new_indent < current {
                // Place the Dedent at the end of the block it closes, not at the next token, so
                // a declaration's span stops at its own last token rather than running on
                // through trailing blank lines and comments.
                while indent_stack.last().copied().unwrap_or(0) > new_indent {
                    indent_stack.pop();
                    out.push((Token::Dedent, prev_end..prev_end));
                }
                if indent_stack.last().copied().unwrap_or(0) != new_indent {
                    diagnostics.push(Diagnostic::error(
                        format!(
                            "indentation does not match any enclosing block (got {new_indent} columns; valid levels are {indent_stack:?})"
                        ),
                        line_start_in_source..span.start,
                    ));
                }
            }
        } else if i == 0 && span.start > 0 {
            // The file's first token is indented; top-level declarations start at column 0.
            if gap.contains('\t') {
                diagnostics.push(Diagnostic::error(
                    "tab characters are not allowed in indentation; use spaces",
                    0..span.start,
                ));
            } else if gap.chars().all(char::is_whitespace) {
                diagnostics.push(Diagnostic::error(
                    "unexpected leading indentation; top-level declarations must start at column 0",
                    0..span.start,
                ));
            } else {
                // Unreachable while the lexer skips everything else; a guard against lexer changes.
                diagnostics.push(Diagnostic::error(
                    "unexpected leading content before first token",
                    0..span.start,
                ));
            }
        }

        match token {
            Token::LParen => paren_depth += 1,
            Token::RParen => paren_depth = (paren_depth - 1).max(0),
            _ => {}
        }

        out.push((token.clone(), span.clone()));
        prev_end = span.end;
    }

    // Close blocks still open at end of input.
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
