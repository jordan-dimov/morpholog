//! `morpholog hash` - a stable content hash of a programme's rules.
//!
//! The hash is SHA-256 over the *canonical source*: the formatter's
//! rendering of the parsed programme. The formatter/parser round-trip
//! property makes that rendering a canonical form, so formatting-only
//! edits do not change the hash, and the hashed artefact is a `.morph`
//! file a human can print and inspect - not internal IR bytes coupled
//! to struct layouts. Comments do not survive canonicalisation, so
//! this is **rules-identity, not file-identity**: editing the teaching
//! prose leaves the hash alone, editing a rule does not. That is the
//! correct semantics for a `ruleset_version` recorded in deployment
//! metadata and evidence packs.
//!
//! Only a valid programme has a hash: an artefact that fails
//! validation is not a ruleset anyone should be versioning against.

use crate::SourceFileArgs;
use crate::commands::{parse_or_exit, print_json, validate_or_exit};

/// The canonical content hash: `sha256:<hex>` over the formatter's
/// canonical rendering. Rules identity, shared with the scorer's
/// `program_hash` and the model hash in `schema`/`generate`.
pub(crate) use morpholog_core::format::canonical_hash;

pub(crate) fn run(args: SourceFileArgs) -> anyhow::Result<()> {
    let parsed = parse_or_exit(&args.file)?;
    // Validation is the gate, not an input: only a valid programme
    // gets an authoritative hash.
    validate_or_exit(&parsed);
    print_json(&morpholog_cli::envelopes::HashReport {
        hash: canonical_hash(&parsed.program),
        program: parsed.program.name.clone(),
    })
}
