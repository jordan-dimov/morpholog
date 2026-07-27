# Copilot review instructions for Morpholog

Morpholog is a programming language and runtime for **invariant-governed
business systems** (finance, trading, regulated workflows). It is **not**
a CRUD application, ORM, generic rules engine, or web framework. Treat
proposed changes accordingly.

The canonical sources of doctrine are:

- [`README.md`](../README.md) - what Morpholog is for, what it answers.
- [`docs/scope-and-ambition.md`](../docs/scope-and-ambition.md) - what's in scope, what's deliberately out.
- [`docs/runtime-semantics.md`](../docs/runtime-semantics.md) - IR + runtime kernel semantics.
- [`docs/design-history.md`](../docs/design-history.md) - which worked example forced which IR primitive.
- Per-example `README.md` under `examples/`.

If a change interacts with the IR (`Invariant`, `Transformation`, `Stmt`,
`Prop`, `ValueExpr`, `Term`, `Value`, `Claim`, `Intent`, `DerivedClaim`, `Transition`,
the declaration types `PredicateDecl` / `IntentDecl` / `ArgDecl`), the
runtime kernel, or the persistence adapter, prefer reading these docs
over inferring intent from surrounding code.

## A few rules code review doesn't get from the linter

- **Terminology: "claims," not "facts."** A `Claim` is an *admitted
  assertion*, viewpoint-dependent, not objective truth. PR text and
  doc comments that say "fact" are drifting and should be flagged.
- **Never `skip_validation`, `force`, or bypass flags.** Exceptions,
  when needed, must be first-class typed claims with full audit
  standing.
- **ASCII-only dashes** in all prose (docs, comments, commit messages,
  PR bodies). No em-dashes (U+2014) or en-dashes (U+2013). Use `-`.
- **Don't pin counts that change** ("the four examples," "the three
  invariants") in docs or doc comments. List the names, or omit the
  count. Test assertions on counts are fine; prose is not.
  **A measurement is not such a count.** "~734 bytes per witness",
  "890 ns against 414 ns", "three of the first four asks dissolved" are
  records of something observed at a moment; they do not drift as the
  project grows, and they are usually the load-bearing part of the
  sentence. Softening one to avoid a numeral removes the evidence. The
  rule is about the size of a set that grows, not about facts.
- **`unsafe_code = "forbid"`** at the workspace level - any
  introduction is a structural change requiring justification.

## Validation

CI is `.github/workflows/ci.yml`. The local equivalent is:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked
cargo audit                                                                          # needs `cargo install cargo-audit` once
cargo test -p morpholog-core -p morpholog-examples -p morpholog-surface -p morpholog-test-support -p morpholog-cli --all-targets --locked

# Against a local PostgreSQL 18+ with crates/morpholog-core/sql/schema.sql applied:
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres -p morpholog-outbox --all-targets --locked -- --test-threads=1
```
