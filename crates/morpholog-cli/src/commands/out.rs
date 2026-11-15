//! Standard output, as every command writes it.
//!
//! Rust's `println!` panics when the reader has closed the pipe, and the
//! default SIGPIPE disposition cannot be restored without unsafe code. So
//! every command writes through [`stdout`], which remembers a write the
//! closed pipe refused. The process then ends as a pipe's writer does,
//! quietly with status 141, whatever error the command made of it, so a
//! truncated stream never reads as complete under `set -o pipefail`. Any
//! other write failure stays an error, and a broken pipe anywhere else (a
//! database socket) is not this one.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

static CLOSED: AtomicBool = AtomicBool::new(false);

/// The status a process ends with when its reader closed standard output:
/// 128 plus SIGPIPE, as the shell reports a writer the signal ended.
pub(crate) const EXIT_STDOUT_CLOSED: u8 = 141;

/// Standard output, locked for as long as the handle lives.
pub(crate) fn stdout() -> Stdout {
    Stdout(io::stdout().lock())
}

/// Whether a reader closed standard output under one of this process's
/// writes.
pub(crate) fn closed() -> bool {
    CLOSED.load(Ordering::Relaxed)
}

pub(crate) struct Stdout(io::StdoutLock<'static>);

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        noted(self.0.write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        noted(self.0.flush())
    }
}

fn noted<T>(result: io::Result<T>) -> io::Result<T> {
    if let Err(e) = &result
        && e.kind() == io::ErrorKind::BrokenPipe
    {
        CLOSED.store(true, Ordering::Relaxed);
    }
    result
}
