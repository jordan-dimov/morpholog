# Morpholog MVP: cut line

Status: superseded historical record. The MVP threshold has been crossed; this doc is kept for the record of what the threshold was and which PRs crossed it.

## The threshold

A developer who has not edited Morpholog's Rust source can:

1. Define a small governed domain model (a set of invariants and named transformations).
2. Run those transformations against a PostgreSQL database via the runtime's commit path.
3. Inspect the admitted claims, audit rows, and pending outbox intents via the CLI.

## How it was crossed

- **PR #17** added the `Program` container in `morpholog-core` and a `program()` constructor on each built-in example, so a "model" became a stable named artefact rather than a bag of Rust functions.
- **PR #18** added `morpholog propose <program> <transformation> --args '<json>' --database-url <url>`, which looks up a built-in program, parses arguments as `Vec<EvalValue>` via the existing codec, and runs `propose_against_pg`. Outcome is JSON on stdout.

After PR #18 a developer can commit governed state against PostgreSQL without writing Rust, for any of the built-in programs. User-supplied programs (i.e. programs not compiled into the `morpholog-cli` binary) remain a follow-on once a parser exists.

## What was deliberately out of scope

These were named as out-of-MVP and remain so unless a worked example forces them:

- Parser / surface syntax. `.morph` files are illustrative; a parser silently commits to file layout, module system, predicate-declaration syntax, diagnostics, error spans, literal syntax, and versioning, and was not worth fossilising before the operational surface was settled.
- As-of evaluation.
- Outbox delivery worker.
- Migrations framework.
- A generic query DSL beyond `inspect`.
- A general module system; cross-program reference.
- Host-language escape inside transformations (out forever - decidable core fragment).

Derived claims were named here as post-MVP and have since landed in Example 5 (see `docs/forced-by-examples.md`).
