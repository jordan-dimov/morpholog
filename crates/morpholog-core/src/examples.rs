//! Worked examples — the IR for the canonical Morpholog illustrations.
//!
//! These constructors serve double duty as both test fixtures (used by
//! `morpholog-core`'s own tests and by `morpholog-postgres` integration
//! tests) and as the canonical illustrations of what Morpholog programs
//! look like as IR data. The corresponding surface-syntax `.morph` files
//! live under `examples/<dir>/<example>.morph`.
//!
//! **Not a stable API.** This module is `pub` so integration tests and
//! documentation can share canonical IR fixtures; it is *not* a
//! user-facing API and it is *not* the future surface language. The
//! shapes here may change as the IR evolves, and the module may be
//! moved behind a feature flag or out of the public surface entirely
//! once a real parser and example-loading mechanism exist. Treat these
//! as teaching fixtures, not a contract.

pub mod claim_standing;
pub mod revenue_restatement;
pub mod settlement_netting;
