//! Morpholog CLI skeleton.
//!
//! Currently prints the package version only. Real subcommands arrive
//! when there is something for them to drive — most importantly a
//! surface parser. Parser, CLI subcommands, and migrations framework
//! remain deliberately deferred per the project's "smallest possible
//! increment" discipline (see `CLAUDE.md` and `docs/scope-and-ambition.md`).

fn main() {
    println!("morpholog {}", env!("CARGO_PKG_VERSION"));
}
