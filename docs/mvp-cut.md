# Morpholog MVP: cut line

Status: decision record. Short, concrete, brutally specific. Not a roadmap, not a doctrine doc, not a marketing brief. A pin against drift.

## What MVP means

A developer who has not edited Morpholog's Rust source can:

1. Define a small governed domain model (a set of invariants and named transformations).
2. Run those transformations against a PostgreSQL database via the runtime's commit path.
3. Inspect the admitted claims, audit rows, and pending outbox intents via the CLI.

That is the threshold. Either it is crossed or it is not. There is no interpretive ambiguity.

## Where we are against that threshold

Today, items 2 and 3 work. The kernel and PostgreSQL adapter already enforce commit legitimacy; the read-side helpers and `morpholog inspect` already expose what was admitted.

Item 1 is the missing piece. Until the introduction of the [`Program`](../crates/morpholog-core/src/lib.rs) container in PR #17, the Morpholog "model" was implicit: a bag of Rust functions inside an `examples` module, only reachable from within Rust tests. Now there is a named container, but the runtime still has no path that lets an external caller select a program and run a named transformation against it. That is the work remaining.

## In scope (for MVP)

- `Program` container in `morpholog-core` packaging a set of invariants and named transformations under a stable identifier. **Landed (PR #17).**
- A `program()` constructor on each built-in example. **Landed (PR #17).**
- CLI invocation of a named transformation from a named program, with arguments supplied as JSON: `morpholog propose <program> <transformation> --args '<json>' --database-url <url>`. The runtime looks up the named program (one of the built-in examples), looks up the named transformation, parses arguments as `Vec<EvalValue>` via the existing codec, opens a SERIALIZABLE PG transaction, and runs the existing `propose_against_pg`. Outcome is serialised as JSON to stdout. **This PR.**

After the pending piece lands, item 1 of the threshold is satisfied for *built-in* programs. That is the MVP threshold. User-supplied programs (i.e. programs not compiled into the `morpholog-cli` binary) are a follow-on once the parser exists.

## Out of scope (for MVP)

These are explicitly deferred. Each is interesting; none is required to cross the threshold.

- **Parser.** `.morph` files remain illustrative until the operational surface is known. A parser is not one feature; it silently commits to file layout, module system, predicate-declaration syntax, diagnostics, error spans, literal syntax, and versioning. Building it before the operational surface is settled would fossilise the wrong choices.
- **Derived claims and read-side projections.** Named on the roadmap as the next semantic frontier after MVP; pinned for a post-MVP example to force the shape.
- **As-of evaluation.** Same reasoning. The audit log already contains enough information; the query primitive can wait.
- **Outbox worker.** Pending intents are visible via `morpholog inspect outbox` already; an actual delivery loop is a separate concern that does not change commit legitimacy.
- **Migrations framework.** Schema is hand-applied; there is one schema. Migrations are a real concern but not MVP.
- **Generic query DSL beyond `inspect`.** The read helpers are `SELECT *` over the three governed tables, ordered deterministically; that is enough to support MVP. Anything richer is post-MVP and probably routes through derived claims.
- **Host-language escape inside transformations.** Out forever (decidable core fragment).
- **A general module system.** Programs are flat in v0. Cross-program reference is not needed for MVP.
- **File loading.** Programs come from the built-in examples first. Loading from `.morph` source is post-parser, which is post-MVP.

## Sequence

Two PRs total. The first has landed.

1. **PR #17:** `Program` struct in `morpholog-core` + per-example `program()` constructors + the original version of this doc.
2. **Final MVP PR (this PR):** CLI invocation. `morpholog propose <program> <transformation> --args '<json>' --database-url <url>`. Looks up the named program and transformation, parses args as `Vec<EvalValue>` via the existing codec, calls `propose_against_pg`, prints the outcome as JSON.

After step 2, the MVP threshold is crossed. A human can commit governed state against PostgreSQL without writing Rust. Subsequent work (parser, derived claims, anything else) is post-MVP and is not constrained by this document.

The original three-PR sequence had a separate CLI discovery step in the middle (`morpholog examples` / `morpholog example <name>`). It was dropped after first-pass review: discovery of built-in programs is what the per-example READMEs and `clap --help` already provide, and a dedicated subcommand for it has no precedent in mainstream language tooling (`rustc`, `python`, `cargo`, `go`, `psql` all defer this to documentation). Adding it would have been CLI ceremony without an actual customer.

## Why this cut line and not another

The alternative cut lines below were considered and rejected:

- *Parser-first MVP.* Rejected because a parser is not one feature; it commits to too many design decisions before the operational surface is settled. Better to expose the existing IR through `Program` and a tiny CLI, watch how it gets used, and let the surface syntax catch up to the operational reality rather than dictate it.
- *Derived-claims-first MVP.* Rejected because derived claims are a semantic expansion that should be forced by an example, not added speculatively. Without read-side projections you can still cross the operability threshold; without operability, derived claims would be implementing for an audience that does not yet exist.
- *Doctrine-document-first MVP.* Rejected because the project already has enough doctrine (`scope-and-ambition.md`, `runtime-semantics.md`, `forced-by-examples.md`). One more abstract essay would add prose gravity without changing what the runtime can do. This document is the smallest doctrinal addition that fits: a single page recording the operational threshold and the PRs that cross it.
- *Discovery-first MVP.* Originally scheduled as step 2 of a three-PR sequence (`morpholog examples` to list built-in programs, `morpholog example <name>` to inspect one). Dropped after first-pass review: discovery of built-in programs is what the per-example READMEs and `clap --help` already provide, with richer context than any CLI listing could. No mainstream language tool ships a "list built-in examples" subcommand for the same reason.

## What this document is not

- Not a roadmap. Post-MVP work (parser, derived claims, as-of, outbox worker, migrations, more examples) is not covered here. That work lives in `scope-and-ambition.md`'s three-level expansion ladder and in `forced-by-examples.md`'s retrospective record.
- Not a feature spec. The "in scope" items above name what the PRs deliver, not how they are designed. Design decisions stay in commit messages, PR descriptions, and per-PR review.
- Not permanent. Once MVP is crossed, the next planning artefact (whatever shape it takes) supersedes this one. Until then, this document is the appeal mechanism for any "should we add X before MVP?" question. The answer is almost always "no, after."
