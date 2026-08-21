//! `morpholog hash` - a stable content hash of a programme's rules.
//!
//! The hash is SHA-256 over a *stable positional canonicalisation* of
//! the parsed programme - a fixed rendering the round-trip property
//! makes canonical, deliberately separate from `format_program`'s
//! evolving human-facing form (which prints named claim patterns). So
//! formatting-only edits do not change the hash, equivalent surface
//! sugar does not either (a named pattern and its positional twin
//! share one hash), and the hashed artefact is still a `.morph` text a
//! human can print and inspect - not internal IR bytes coupled to
//! struct layouts. Comments do not survive canonicalisation, so this
//! is **rules-identity, not file-identity**: editing the teaching
//! prose leaves the hash alone, editing a rule does not. That is the
//! correct semantics for a `ruleset_version` recorded in deployment
//! metadata and evidence packs.
//!
//! Only a valid programme has a hash: an artefact that fails
//! validation is not a ruleset anyone should be versioning against.

use crate::SourceFileArgs;
use crate::commands::{parse_or_report, print_json, validate_or_report};

/// The canonical content hash: `sha256:<hex>` over the stable
/// positional canonicalisation (not `format_program`'s evolving human
/// form). Rules identity, shared with the scorer's `program_hash` and
/// the model hash in `schema`/`generate`.
pub(crate) use morpholog_core::format::canonical_hash;

pub(crate) fn run(args: SourceFileArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    // Validation is the gate, not an input: only a valid programme
    // gets an authoritative hash.
    validate_or_report(&parsed)?;
    print_json(&morpholog_cli::envelopes::HashReport {
        hash: canonical_hash(&parsed.program),
        program: parsed.program.name.clone(),
    })
}
