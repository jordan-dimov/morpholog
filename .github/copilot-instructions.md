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

## Doctrine lines a review should check, each with the test that pins it

These are the lines reviews have caught crossed. A change near one of
them should either keep the pinning test green or move it deliberately.

- A verifier **policy flag never masks the intrinsic verdict**: a broken
  pack reports as broken with or without `--require-signatures`,
  `--require-signatures-from`, or `--require-signing-key`
  (`the_pin_never_masks_a_sparse_packs_intrinsic_verdict`). A pin is an
  intersection with the log's own key authority, never a substitute.
- A `check --json` report has **one `file`**, so a finding about another
  file (an `--against` programme) carries no span
  (`against_json_carries_the_finding_with_the_local_span_and_no_foreign_spans`);
  the plain renderer may show that file's own carets.
- A proposal row can carry **only a published proposal code**
  (`ProposeCode`, held to `propose_error_code` in `result.json` by the
  contract test); the session-only codes never reach a batch receipt.
- Every surface an embedder consumes is a **pinned envelope**: a `$defs`
  entry in `result.json`, a golden under `tests/golden/envelopes/`, and
  the generated Python client, moving together. Ad-hoc JSON on a
  consumed surface is drift.
- A migration **refuses a shape it does not recognise** rather than
  declaring it current, and never replaces a function that generated
  stored values (`012_claims_hash_key` is the model).

## Validation

CI is `.github/workflows/ci.yml`. The local equivalent is
`./scripts/precommit.sh`, run in two steps: `env -u DATABASE_URL
./scripts/precommit.sh` first (formatting, clippy, rustdoc, the Rust
floor, `cargo audit`, the sync suites, the Python client), then the
full run with `DATABASE_URL` set. The PostgreSQL-backed suites need a
disposable **cluster**, not just a database - they truncate the schema
and create and drop cluster-global roles - so `CONTRIBUTING.md` puts
development on a second cluster (`postgres:///morpholog_dev?port=55432`).
