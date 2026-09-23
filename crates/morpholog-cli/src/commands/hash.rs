//! `morpholog hash` - a stable content hash of a programme's rules.
//!
//! SHA-256 over a fixed positional rendering of the parsed programme,
//! kept separate from `format_program`'s human-facing form, which may
//! change. Formatting edits and equivalent sugar (a named pattern and its
//! positional twin) keep the hash, and the hashed text is still readable
//! `.morph`, not IR bytes.
//!
//! Comments are dropped, so this is **rules identity, not file identity**:
//! editing prose keeps the hash, editing a rule changes it. That is what a
//! `ruleset_version` in deployment metadata and evidence packs needs.
//!
//! Only a valid programme gets a hash.

use crate::SourceFileArgs;
use crate::commands::{parse_or_report, print_json, validate_or_report};

/// The canonical rules hash, `sha256:<hex>`. The same value as the
/// scorer's `program_hash` and the model hash in `schema`/`generate`.
pub(crate) use morpholog_core::format::canonical_hash;

pub(crate) fn run(args: SourceFileArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    validate_or_report(&parsed)?;
    print_json(&morpholog_cli::envelopes::HashReport {
        hash: canonical_hash(&parsed.program),
        program: parsed.program.name.clone(),
    })
}
