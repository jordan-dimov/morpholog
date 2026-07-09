//! Envelope structs shared between the `morpholog` binary and its
//! contract tests. The binary serializes these directly; the contract
//! suite constructs the same types and pins the bytes against the
//! goldens, so a field change is a compile error on both sides rather
//! than silent producer/golden drift.

pub mod envelopes;
