//! `morpholog session` - the resident runtime over stdio. It answers
//! NDJSON requests on stdin with the same envelopes the one-shot commands
//! print, one line per request, in order.
//!
//! The protocol is lockstep: one request in, one response out, no
//! correlation ids. A propose answers with the batch receipt (`row` is the
//! 1-based request line); reads answer with the claim arrays. A
//! per-request failure answers with an error receipt carrying a stable
//! `code`. A proposal's database failure is a receipt too:
//! `not_committed` when nothing was recorded, `commit_outcome_unknown`
//! when COMMIT failed without a verdict.
//!
//! The process aborts, non-zero, only on a failure that cannot be a
//! receipt: a broken stream, an operational failure on a read, or a
//! proposal's receipt that could not be serialised after the adapter
//! returned. To a caller with a request in flight, an abort means the
//! outcome is unknown; the generated client reports it as such and never
//! retries silently. Everything the binary knows did not commit is a coded
//! receipt, never an abort.
//!
//! The programme, the connection and the dispatch are the resident
//! runtime's (`commands::resident`); this module is the stdio loop around
//! it. The ready line's `model_hash` tells a caller which programme the
//! runtime serves, and `--expect-hash` refuses to start under any other.

use anyhow::Context;
use std::io::{BufRead, Write};

use crate::SessionArgs;
use crate::commands::resident::{Permits, Resident, ResidentFailure};
use morpholog_cli::envelopes::ErrorReceipt;

/// A runaway guard, not a working limit. A longer request line aborts the
/// session: a half-read line cannot be resynchronised, so it cannot be a
/// receipt.
const MAX_REQUEST_LINE: usize = 64 * 1024 * 1024;

pub(crate) async fn run(args: SessionArgs) -> anyhow::Result<()> {
    let resident = Resident::start(
        &args.file,
        &args.db.database_url,
        args.expect_hash.as_deref(),
    )
    .await?;

    // The stdio caller started the process, so it already holds everything
    // the process holds: there is nothing to withhold from it.
    let permits = Permits::all();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    write_line(&mut out, resident.ready())?;

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut line = String::new();
    let mut row: u64 = 0;
    loop {
        line.clear();
        if read_line_capped(&mut input, &mut line)? == 0 {
            break; // EOF: the clean shutdown.
        }
        row += 1;
        if line.trim().is_empty() {
            continue; // Skipped without a receipt, like a batch blank.
        }
        match resident.handle(line.trim(), row, &permits).await {
            Ok(response) => write_line(&mut out, &response)?,
            Err(ResidentFailure::Request { code, reason }) => {
                let receipt = ErrorReceipt::new(
                    code,
                    format!("{reason:#}"),
                    crate::commands::propose::kernel_kind(&reason),
                    row,
                );
                write_line(&mut out, &serde_json::to_value(&receipt)?)?;
            }
            Err(ResidentFailure::Operational(reason)) => {
                out.flush()?;
                return Err(reason.context(format!("operational failure at request {row}")));
            }
        }
    }
    out.flush()?;
    Ok(())
}

/// Write one compact response line and flush it: the caller is waiting on
/// it, so buffering would deadlock.
fn write_line(out: &mut impl Write, value: &serde_json::Value) -> anyhow::Result<()> {
    writeln!(out, "{}", serde_json::to_string(value)?).context("writing a response line")?;
    out.flush().context("flushing a response line")?;
    Ok(())
}

/// `read_line` with the runaway guard, so input with no newline cannot
/// allocate without bound. Decodes UTF-8 once per whole line, since a
/// character can straddle two buffer fills. Returns the bytes read; 0 is
/// EOF.
fn read_line_capped(input: &mut impl BufRead, line: &mut String) -> anyhow::Result<usize> {
    let mut bytes = Vec::new();
    loop {
        let chunk = input.fill_buf().context("reading a request line")?;
        if chunk.is_empty() {
            break; // EOF (possibly mid-line; the trim handles it).
        }
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (chunk.len(), false),
        };
        if bytes.len() + take > MAX_REQUEST_LINE {
            anyhow::bail!(
                "a request line exceeded {MAX_REQUEST_LINE} bytes; a half-read line \
                 cannot be resynchronised, so the session aborts"
            );
        }
        bytes.extend_from_slice(&chunk[..take]);
        input.consume(take);
        if done {
            break;
        }
    }
    line.push_str(std::str::from_utf8(&bytes).context("a request line is not valid UTF-8")?);
    Ok(bytes.len())
}

#[cfg(test)]
mod tests;
