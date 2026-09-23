//! Envelope structs shared by the `morpholog` binary and its contract
//! tests. Both sides use the same types, so a field change breaks the build
//! instead of drifting silently from the goldens.
//!
//! Not a stable public API. Embedders consume the pinned JSON envelopes
//! (`schema --result`, the generated client), never these types.

pub mod envelopes;
