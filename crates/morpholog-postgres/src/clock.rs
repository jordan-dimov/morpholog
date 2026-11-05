//! Where a read of the record spends its time, for the bench: the page
//! queries, the decoding of their rows, and a rehearsal's decisions and
//! advances. Totals across the process, read before and after a call.
//! An ordinary build has none of it.

/// A span of the read or the fold.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Span {
    /// A page query, through the driver's rows.
    Fetch = 0,
    /// Those rows decoded into audit rows.
    Decode = 1,
    /// A rehearsal's decision on one row.
    Decide = 2,
    /// The replay state advanced by one row's recorded effect.
    Advance = 3,
}

#[cfg(feature = "measure")]
mod on {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use super::Span;

    // Atomics rather than a thread's own counters: an async read can
    // resume on another thread.
    static NANOS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

    pub(crate) struct Started(Instant);

    pub(crate) fn start() -> Started {
        Started(Instant::now())
    }

    pub(crate) fn stop(span: Span, started: Started) {
        let nanos = u64::try_from(started.0.elapsed().as_nanos()).unwrap_or(u64::MAX);
        NANOS[span as usize].fetch_add(nanos, Ordering::Relaxed);
    }

    /// The totals so far: fetch, decode, decide, advance.
    pub fn snapshot() -> [Duration; 4] {
        NANOS
            .each_ref()
            .map(|n| Duration::from_nanos(n.load(Ordering::Relaxed)))
    }
}

#[cfg(feature = "measure")]
pub use on::snapshot;
#[cfg(feature = "measure")]
pub(crate) use on::{start, stop};

#[cfg(not(feature = "measure"))]
pub(crate) struct Started;

#[cfg(not(feature = "measure"))]
#[inline(always)]
pub(crate) fn start() -> Started {
    Started
}

#[cfg(not(feature = "measure"))]
#[inline(always)]
pub(crate) fn stop(_span: Span, _started: Started) {}
