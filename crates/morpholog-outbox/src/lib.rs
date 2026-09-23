//! Polling loop over `morpholog-postgres`'s single-row outbox processor.
//!
//! `morpholog_postgres::process_one_outbox_row` handles one row. This crate adds a drain that
//! processes all due rows in one pass, and a background worker that polls with jittered sleep,
//! wakes early for scheduled retries, and stops on a shutdown signal.
//!
//! It is a separate crate so a deployment that drives the processor its own way (a Lambda, a
//! Kafka consumer) can depend on `morpholog-postgres` alone, without tokio or a polling worker.

#![forbid(unsafe_code)]

pub mod clock;
mod deliverers;
mod drain;
pub mod jitter;
pub mod testing;
mod worker;

pub use clock::{Clock, RealClock};
pub use deliverers::StdoutDeliverer;
pub use drain::process_available_outbox_rows;
pub use jitter::{JitterRng, RandJitter};
pub use worker::OutboxWorker;
