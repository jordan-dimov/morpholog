# Contributing to Morpholog

Notes for developers working on the codebase. For the project's framing and worked-example tour, see [`README.md`](README.md); for the doctrine and roadmap, see [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) and [`docs/roadmap.md`](docs/roadmap.md).

## Prerequisites

Using Morpholog rather than developing it? The prebuilt-binary path is
[`docs/install.md`](docs/install.md); everything below is for working on
the source.

- **Rust 1.95+** (edition 2024). Stable toolchain; `rustup default stable` suffices.
- **PostgreSQL 18+**, system-wide on Ubuntu or equivalent. PG-only; portability is not a goal. The adapter uses SSI, JSONB, and generated columns.
- **`cargo-audit`** for the dependency-vulnerability check: `cargo install cargo-audit`.
- **A pinned nightly and `cargo-public-api`** for the public-API check, which precommit never skips. The exact pins live in [`scripts/public_api.sh`](scripts/public_api.sh), and the script prints both install commands if either is missing. Nightly is used only to render rustdoc JSON; the compiler contract stays stable and the declared floor.
- **`sqlx-cli`** (only when adding or changing a SQL query): the adapter's queries are compile-time-checked against the schema via a committed offline cache, and regenerating it uses the version-matched CLI: `cargo install sqlx-cli --version 0.9.0 --no-default-features --features postgres,rustls`. A normal build needs neither the CLI nor a database (see below).
- **Python 3** (optional): the precommit script runs the generated-client template tests, and (with `DATABASE_URL`) the worked embedder end to end; both are skipped with a note when `python3` is absent. CI pins the generated client's declared floor; any local Python 3 is a smoke test.

No Docker. No additional system dependencies.

## Local setup

The PostgreSQL-backed suites need a **disposable cluster**, not just a disposable database: they truncate whatever `DATABASE_URL` names on entry; several create and drop roles, which are cluster-global; some need the connecting role to be a superuser; and the checkpoint writer census asserts that nothing else in the cluster can write `morpholog.audit` - so a role a *neighbouring* Morpholog deployment granted membership in the cluster-global `morpholog_writer` reddens the census test, correctly, because it is a real privilege relationship. That happened here: a second project on the same machine, with its own database and its own roles, failed this repo's census test twice, and deleting its roles was the wrong remedy.

So development gets a cluster of its own on a second port, and that is the only local setup this guide describes. On Ubuntu:

```bash
git clone https://github.com/jordan-dimov/morpholog.git
cd morpholog
sudo pg_createcluster -p 55432 --start 18 morpholog_test
sudo -u postgres createuser -p 55432 --superuser "$USER"
createdb -p 55432 morpholog_dev
psql -p 55432 morpholog_dev -f crates/morpholog-core/sql/schema.sql
export DATABASE_URL='postgres:///morpholog_dev?port=55432'
```

The socket form keeps peer authentication, so no password is involved; `postgres://localhost:55432/...` would ask for one under the default `pg_hba.conf`. Real Morpholog deployments stay on `:5432`, untouched by any test. To dispose of the cluster: `sudo pg_dropcluster --stop 18 morpholog_test`.

The schema applies the head state from `crates/morpholog-core/sql/schema.sql`. An existing database comes forward with `morpholog migrate` (`--check` to ask first), which carries the numbered migrations under `crates/morpholog-core/sql/migrations/` inside the binary. (An installed `morpholog` binary provisions the same schema with `morpholog init`; the `psql` path is right for a source checkout, where the binary you last installed may trail the schema at head.)

Optional but recommended:

```bash
cargo install --path crates/morpholog-cli
```

That puts the `morpholog` binary on `~/.cargo/bin/`. Refresh it after pulling changes by re-running the same command (cargo no-ops when there's nothing to rebuild).

## Build and test

Run [`./scripts/precommit.sh`](scripts/precommit.sh) before pushing. It runs the suites and checks CI gates on, plus `morpholog check` over every `.morph`; CI additionally runs a coverage job for visibility only, and verifies the declared Rust floor (precommit does the same when that toolchain is installed, and says so when it is not). If it passes locally, CI passes.

```bash
env -u DATABASE_URL ./scripts/precommit.sh   # the fast pass: everything but the PG-backed suites
./scripts/precommit.sh                       # the full run, with DATABASE_URL exported as above
```

Run them in that order. The fast pass takes a fraction of the time and catches most of what fails a full run (formatting, clippy, rustdoc, the sync suites); the full run is then paid once. A full run restarted for a formatting slip is ten minutes lost.

The script bails on the first failure. Without `DATABASE_URL` it skips the PG-backed test suites with a note; with it set, it runs them against whatever the URL names, which is why the URL above points at the disposable cluster.

The underlying commands (CI runs the same in `.github/workflows/ci.yml`):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked
cargo audit
cargo test -p morpholog-core -p morpholog-examples -p morpholog-surface -p morpholog-test-support --all-targets --locked
cargo test -p morpholog-cli -p morpholog-postgres -p morpholog-outbox -p morpholog-bench --all-targets --locked -- --test-threads=1   # with DATABASE_URL exported
# when python3 is available:
python3 -m unittest discover crates/morpholog-cli/templates/python_client/tests
```

The PG-backed test suites share one schema and truncate it between tests; they must run serially (`--test-threads=1`). The `morpholog-bench` entry is its N=1 compatibility smoke test, not a scale run.

### Public Rust API

The supported public surface of each library crate an embedder can depend on (what rustdoc shows; `#[doc(hidden)]` items are not supported and not listed) is committed as text under [`api/`](api/), one file per crate, rendered by `cargo public-api`. `./scripts/public_api.sh` regenerates each snapshot and fails on any difference, in precommit and in CI. When you change a public item on purpose (a signature, a field, a variant, a new `pub`), run `./scripts/public_api.sh --update` and commit the snapshot with the change: the diff of `api/` is how the PR says it changed the API, and a reviewer reads it. The crate list in the script is explicit, so an internal crate never joins the contract by accident.

### Compile-time-checked SQL

The persistence adapter's queries are `sqlx::query!` / `query_as!` macros, verified against the real schema **at build time**. A query that drifts from the schema is a compile error, not a runtime surprise. Every cargo command in this workspace defaults to `SQLX_OFFLINE=true` (via [`.cargo/config.toml`](.cargo/config.toml)), so a plain `cargo build` - and precommit, and CI - reads the committed cache in `.sqlx/` and needs no database.

When you add or change a query, regenerate the cache with `DATABASE_URL` exported as above and commit the result:

```bash
./scripts/sqlx-prepare.sh   # against the same disposable database; it drops and recreates the schemas
git add .sqlx
```

**Never hand-edit `.sqlx/`.** Those files are a generated contract between the queries and the schema; the only correct way to change them is to regenerate via the script above. A hand-edit that disagrees with the schema is exactly the drift the cache exists to catch.

The checked contract is **PostgreSQL 18**, the stated floor: regenerate against a clean PG 18 database when you have one, and CI's PG 18 `cargo sqlx prepare --workspace --check` is the source of truth for floor compatibility. Precommit and CI run that check against the live schema to fail when the cache and the schema have drifted apart - so a forgotten regen is caught before merge.

## Workspace layout

```
crates/
  morpholog-cli/           # binary `morpholog`; file-path-driven CLI (see `morpholog --help`)
  morpholog-core/          # synchronous semantic kernel - no I/O
  morpholog-examples/      # worked examples parsed from .morph + typed test fixtures (depends on core)
  morpholog-postgres/      # async PostgreSQL persistence adapter
  morpholog-outbox/        # polling outbox worker
  morpholog-witness/       # external timestamp witnesses for audit checkpoints (RFC 3161)
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
- **ASCII-only dashes** in `.md`, `.rs`, and `.morph` files. Never em-dash (U+2014) or en-dash (U+2013). Both the precommit script and CI enforce this, over every tracked `.md`, `.rs`, and `.morph` file. Em-dashes render unreliably across terminals and clutter `grep` / `diff`.
- **No bypass flags ever.** Anything resembling `skip_validation`, `force_commit`, or `--no-verify`-style escape hatches is rejected at review. Exceptions are first-class typed claims with full audit standing.

## Comments and prose

Comments and docs earn their place by the same subtraction test as code. This is a review pass, not a gate: "chatty" and "self-documenting" are judgment calls, not lints, which is why they live here and not in `precommit.sh` - a comment-density check would only punish the thorough rustdoc the next point asks for.

- **A comment explains WHY; the code already says WHAT.** Before writing one, reach for a better name, a smaller function, or a type that makes it unnecessary. If deleting the comment would not confuse a future reader, delete it.
- **rustdoc is the exception** - precise and complete on every public item, since it is the API contract. The WHY-not-WHAT bar is for inline `//` comments, not `///` docs.
- **One concept, one home.** Each doc has a single role (see [Reference](#reference)); explain a thing where it belongs and link to it rather than restating it - duplicated prose drifts out of sync.
- **Audience prose evokes the why** (README, `docs/`, example READMEs): lead with the question Morpholog answers, not the feature list.
- **History compresses as it ages.** `design-history.md` entries distil to Forced-by/Landed stubs once the work settles; git holds the per-PR detail. The cadence is the release: at each one, every entry that landed before the previous release and still runs past roughly 350 words is cut to its stub - keep the decisions and the refutations (what was tried and why it was wrong), drop the blow-by-blow.

## Adding code

- **Smallest possible increment that produces a working artefact.** Three lines compiling beat a sketch of the whole subsystem.
- **Kernel primitives land alongside the worked example that forces them.** Speculative IR primitives are explicitly discouraged - see [`docs/design-history.md`](docs/design-history.md) for the pattern.
- **Higher-level functional tests over low-level unit tests.** A test that exercises `propose`, `propose_against_pg`, or the `morpholog` CLI catches more real regressions than a test of an individual private function.
- **Structural questions use the shared walk; semantic ones match the IR directly.** "Does this subtree contain X", "which names does it bind", "how many nodes" go through `morpholog_core::fold` in the kernel and `parser::walk` in the surface, never a fresh recursion - copied descents have drifted before. An evaluator, checker, formatter, or compiler matches the IR itself, because there the match is the rule: each arm means something different and hiding it behind a generic visitor would hide the semantics. The test of which you are writing: if every arm does the same thing but at the leaves, it is structural.
- **`bind` / `admit` / `retract` / `emit` accept claim patterns at the surface.** The IR is more permissive for some of these; the parser refuses to produce IR the kernel will refuse to evaluate. See `crates/morpholog-surface/src/parser/stmt.rs` for the doctrine.

## Adding a worked example

A worked example is **its `.morph`**. Put the canonical `.morph` source and the business framing (`README.md`) in `examples/NN_<name>/`, add one line - `example_module!(<name>);` - to `crates/morpholog-examples/src/lib.rs`, a row to the capability index in [`examples/README.md`](examples/README.md), and an entry to the main README's domain list.

None of those can be silently forgotten. The generated `all_programs()` registry references the module, so a missing `example_module!` line is a **compile error**, not a quietly skipped example (the failure mode the old hand-maintained list had). The `capability_index` test refuses an example missing from either README, and checks that each index row's construct actually appears in the example it points at - an embedder once spent a week designing around a limitation that did not exist because the example demonstrating it was named after a different business. `crates/morpholog-examples/build.rs` reads the `.morph`, extracts its `transformation` / `invariant` / `derived` declarations, and generates the accessor module (`program`, `all_invariants`, one getter per declaration) that the tests use as fixtures; the cross-example property tests pick the example up through that registry. There is no hand-written accessor module and no IR by hand. If a test needs a domain-symbol constant the `.morph` cannot yield (a role name, a list name) or an accessor for a *generated* discipline invariant, add it as a small supplement in that `example_module!` call's `{ ... }` body.

The `.morph` file is the **single source of truth**, not an illustration: the runnable program *is* the parsed teaching source, so the two cannot drift - there is no separate hand-built IR to keep in sync. A pedagogical simplification belongs in the `README` or in `.morph` comments, never in a divergent model. (The round-trip test in `morpholog-surface` checks `format(IR) -> parse == IR` over generated source, and the CLI integration test verifies every example parses and validates.)

The `.morph` comments are a **learner's guide**, not implementation notes. Their reader is someone learning Morpholog who is *not* an expert in the example's industry. Open each `.morph` with a header that teaches the domain from scratch in plain, engaging language - the real business problem and why the example is realistic - and weaves in the philosophy: an invariant is a rule the runtime will not let any change break; a `require` is a gate on a single action, checked only at the moment you act; the system stores admitted *claims*, not bare "facts". Annotate each section and non-obvious construct in business terms. Keep language and runtime internals out of `.morph` comments entirely - no type or variant names, no talk of the IR or the kernel; a learner does not care how it is implemented - and never open with a fictional scene; ground it in how the industry actually works. The gold standard is [`examples/10_trade_lifecycle/trade_lifecycle.morph`](examples/10_trade_lifecycle/trade_lifecycle.morph): it teaches an unfamiliar domain from scratch and weaves in the most philosophy - the gate-versus-invariant distinction, lifecycle phase as accumulated claims rather than a status field, and two standings on one figure. [`examples/03_double_entry_ledger/ledger.morph`](examples/03_double_entry_ledger/ledger.morph) is the concise exemplar, for when brevity matters more than breadth of coverage. This is a different audience, and a different job, from the example's `README.md` and from rustdoc (the implementer).

The `README.md` is the example's **browsable face** - what renders when someone lands on the directory. Its job is deliberately *not* the `.morph`'s, and the two must not duplicate. The README gives a **concise** business framing (enough to evaluate what the example governs and why it is interesting), a **program-at-a-glance** (the claims, invariants, and transformations, as a table or short list), **how to run it**, and **what it deliberately does not cover**. The deep teaching - the domain from scratch, and the philosophy at each construct - lives in the `.morph`, so the README does **not** re-teach it: it frames and orients, and a reader who wants the depth opens the `.morph`. The clearest models are [`examples/09_carbon_credit_provenance/README.md`](examples/09_carbon_credit_provenance/README.md), which explicitly delegates the guided domain tour to its `.morph`, and [`examples/13_biometric_identification_oversight/README.md`](examples/13_biometric_identification_oversight/README.md), whose statute-to-rule table *is* the program-at-a-glance. The smell to avoid is a standalone "design notes" section that re-teaches a kernel concept (require-vs-invariant, `pre(...)`, bitemporal time) the `.morph` already owns inline - that is the one place the domain gets taught, not two.

## Reference

- [`docs/developer-intro.md`](docs/developer-intro.md) - the guided first hour: a programme, a database, a proposal, a refusal.
- [`docs/install.md`](docs/install.md) - the prebuilt-binary path and how a deployment upgrades.
- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) - **read first** when reasoning about whether a direction fits the project.
- [`docs/roadmap.md`](docs/roadmap.md) - what's imminent, deferred, and out of scope.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) - what the kernel means.
- [`docs/refactoring-playbook.md`](docs/refactoring-playbook.md) - how to make a codebase-wide type change safely.
- [`docs/benchmarking.md`](docs/benchmarking.md) - the benchmark suite's discipline: what may be made fast, and what may not be made easier.
- [`docs/design-history.md`](docs/design-history.md) - for each significant IR decision, the worked example that forced it.
- [`docs/embedder-integration.md`](docs/embedder-integration.md) - the pinned public contract for non-Rust integrations, including the generated Python client.
- [`docs/prior-art.md`](docs/prior-art.md) - the influences behind the roadmap directions, with the calibrations that survived review.
- [`docs/outbox-sketch.md`](docs/outbox-sketch.md) - the "Morpholog plus an Outside Coordinator" doctrine for the outbox worker.

## License

By contributing you agree that your contributions will be licensed under the [Apache License, Version 2.0](LICENSE).
