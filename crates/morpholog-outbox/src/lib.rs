//! Polling-loop layer over `morpholog-postgres`'s single-row outbox
//! processor.
//!
//! The kernel processor (`morpholog_postgres::process_one_outbox_row`)
//! handles one claim-deliver-route cycle. This crate adds the
//! shapes a deployment actually wants: a loop that drains all due
//! work in one pass, a worker that polls in the background with
//! jittered sleep and a shutdown signal, and a smart-sleep path
//! that honors `next_attempt_at` rather than busy-polling.
//!
//! Why a separate crate, not more code in `morpholog-postgres`:
//! - Different dependency profile. The polling worker pulls in
//!   the tokio runtime and jitter randomness;
//!   `morpholog-postgres` stays a focused adapter crate.
//! - The worker is replaceable. A deployment that wants to drive
//!   the processor from a Lambda invocation, a Kafka-consumer
//!   loop, or no background at all should not be forced to
//!   import a polling worker. They can depend on
//!   `morpholog-postgres` alone and write their own loop.

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
