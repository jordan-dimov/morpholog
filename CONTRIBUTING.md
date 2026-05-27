# Contributing to Morpholog

Notes for developers working on the codebase. For the project's framing and worked-example tour, see [`README.md`](README.md); for the doctrine and roadmap, see [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) and [`docs/roadmap.md`](docs/roadmap.md).

## Prerequisites

- **Rust 1.95+** (edition 2024). Stable toolchain; `rustup default stable` suffices.
- **PostgreSQL 17+**, system-wide on Ubuntu or equivalent. PG-only; portability is not a goal. The adapter uses SSI, JSONB, and generated columns.
- **`cargo-audit`** for the dependency-vulnerability check: `cargo install cargo-audit`.

No Docker. No additional system dependencies.

## Local setup

```bash
git clone https://github.com/jordan-dimov/morpholog.git
cd morpholog
createdb morpholog_dev
psql morpholog_dev -f crates/morpholog-core/sql/schema.sql
export DATABASE_URL=postgres:///morpholog_dev
```

The schema applies the head state from `crates/morpholog-core/sql/schema.sql`. For an existing database, the migrations under `crates/morpholog-core/sql/migrations/` apply in numeric order.

Optional but recommended:

```bash
cargo install --path crates/morpholog-cli
```

That puts the `morpholog` binary on `~/.cargo/bin/`. Refresh it after pulling changes by re-running the same command (cargo no-ops when there's nothing to rebuild).

## Build and test

Run [`./scripts/precommit.sh`](scripts/precommit.sh) before pushing. It runs every check CI runs, in the same order, plus the ASCII-only-dash convention that CI does not enforce. If it passes locally, CI passes.

```bash
./scripts/precommit.sh                                        # without PG tests
DATABASE_URL=postgres:///morpholog_dev ./scripts/precommit.sh # full suite
```

The script bails on the first failure. Without `DATABASE_URL` it skips the PG-backed test suites with a note.

The underlying commands (CI runs the same in `.github/workflows/ci.yml`):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked
cargo audit
cargo test -p morpholog-core -p morpholog-examples -p morpholog-surface -p morpholog-test-support --all-targets --locked
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-cli -p morpholog-postgres -p morpholog-outbox --all-targets --locked -- --test-threads=1
```

The PG-backed test suites share one schema and truncate it between tests; they must run serially (`--test-threads=1`).

## Workspace layout

```
crates/
  morpholog-cli/           # binary `morpholog`; inspect, run, parse, check, explain
  morpholog-core/          # synchronous semantic kernel - no I/O
  morpholog-examples/      # worked-example IR + registry (depends on core)
  morpholog-postgres/      # async PostgreSQL persistence adapter
  morpholog-outbox/        # polling outbox worker
  morpholog-surface/       # .morph parser arc: lexer, layout pass, parser
  morpholog-test-support/  # shared sync test helpers (dev-deps only)
  morpholog-bench/         # scale-pressure benchmark (destructive)
```

`morpholog-core` and `morpholog-examples` are sync and pure. `morpholog-postgres` and `morpholog-outbox` are async (sqlx/tokio). Async at the adapter boundary is fine; **async must not infect the core evaluator or `propose()` API.**

## Conventions

These are project rules, not style preferences:

- **`#[forbid(unsafe_code)]`** at the workspace level. No exceptions.
- **Decimal-first** for business values. Never `f64`/`f32` in financial arithmetic. The codebase uses `rust_decimal` and maps it to PostgreSQL `numeric`.
- **UUIDs are always v7** and opaque to the surface language.
- **ASCII-only dashes** in `.md`, `.rs`, and `.morph` files. Never em-dash (U+2014) or en-dash (U+2013). The precommit script enforces this. Em-dashes render unreliably across terminals and clutter `grep` / `diff`.
- **No bypass flags ever.** Anything resembling `skip_validation`, `force_commit`, or `--no-verify`-style escape hatches is rejected at review. Exceptions are first-class typed claims with full audit standing.

## Comments and prose

Comments and docs earn their place by the same subtraction test as code. This is a review pass, not a gate: "chatty" and "self-documenting" are judgment calls, not lints, which is why they live here and not in `precommit.sh` - a comment-density check would only punish the thorough rustdoc the next point asks for.

- **A comment explains WHY; the code already says WHAT.** Before writing one, reach for a better name, a smaller function, or a type that makes it unnecessary. If deleting the comment would not confuse a future reader, delete it.
- **rustdoc is the exception** - precise and complete on every public item, since it is the API contract. The WHY-not-WHAT bar is for inline `//` comments, not `///` docs.
- **One concept, one home.** Each doc has a single role (see [Reference](#reference)); explain a thing where it belongs and link to it rather than restating it - duplicated prose drifts out of sync.
- **Audience prose evokes the why** (README, `docs/`, example READMEs): lead with the question Morpholog answers, not the feature list.
- **History compresses as it ages.** `design-history.md` entries distil to Forced-by/Landed stubs once the work settles; git holds the per-PR detail.

## Adding code

- **Smallest possible increment that produces a working artefact.** Three lines compiling beat a sketch of the whole subsystem.
- **Kernel primitives land alongside the worked example that forces them.** Speculative IR primitives are explicitly discouraged - see [`docs/design-history.md`](docs/design-history.md) for the pattern.
- **Higher-level functional tests over low-level unit tests.** A test that exercises `propose`, `propose_against_pg`, or the `morpholog` CLI catches more real regressions than a test of an individual private function.
- **`bind` / `admit` / `retract` / `emit` accept claim patterns at the surface.** The IR is more permissive for some of these; the parser refuses to produce IR the kernel will refuse to evaluate. See `crates/morpholog-surface/src/parser/stmt.rs` for the doctrine.

## Adding a worked example

A worked example lives in two places:

1. `examples/NN_<name>/` carries the **canonical `.morph` source** and the business framing as a `README.md`.
2. `crates/morpholog-examples/src/<name>.rs` is a thin accessor module: it embeds the `.morph` via `include_str!`, parses it once into the registered `Program`, and exposes the by-name getters (`program`, `all_invariants`, individual transformations and invariants) the tests use as fixtures. It builds no IR by hand.

The `.morph` file is the **single source of truth**, not an illustration: the runnable program *is* the parsed teaching source, so the two cannot drift - there is no separate hand-built IR to keep in sync. A pedagogical simplification belongs in the `README` or in `.morph` comments, never in a divergent model. (The round-trip test in `morpholog-surface` checks `format(IR) -> parse == IR` over generated source, and the CLI integration test verifies every example parses and validates.)

The `.morph` comments are a **learner's guide**, not implementation notes. Their reader is someone learning Morpholog who is *not* an expert in the example's industry. Open each `.morph` with a header that teaches the domain from scratch in plain, engaging language - the real business problem and why the example is realistic - and weaves in the philosophy: an invariant is a rule the runtime will not let any change break; a `require` is a gate on a single action, checked only at the moment you act; the system stores admitted *claims*, not bare "facts". Annotate each section and non-obvious construct in business terms. Keep language and runtime internals out of `.morph` comments entirely - no type or variant names, no talk of the IR or the kernel; a learner does not care how it is implemented - and never open with a fictional scene; ground it in how the industry actually works. The gold standard is [`examples/10_trade_lifecycle/trade_lifecycle.morph`](examples/10_trade_lifecycle/trade_lifecycle.morph): it teaches an unfamiliar domain from scratch and weaves in the most philosophy - the gate-versus-invariant distinction, lifecycle phase as accumulated claims rather than a status field, and two standings on one figure. [`examples/03_double_entry_ledger/ledger.morph`](examples/03_double_entry_ledger/ledger.morph) is the concise exemplar, for when brevity matters more than breadth of coverage. This is a different audience from the example's `README.md` (the auditor or controller) and from rustdoc (the implementer).

## Reference

- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) - **read first** when reasoning about whether a direction fits the project.
- [`docs/roadmap.md`](docs/roadmap.md) - what's imminent, deferred, and out of scope.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) - what the kernel means.
- [`docs/design-history.md`](docs/design-history.md) - for each significant IR decision, the worked example that forced it.
- [`docs/outbox-sketch.md`](docs/outbox-sketch.md) - the "Morpholog plus an Outside Coordinator" doctrine for the outbox worker.

## License

By contributing you agree that your contributions will be licensed under the [Apache License, Version 2.0](LICENSE).
