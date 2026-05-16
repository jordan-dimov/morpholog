# Copilot Instructions for Morpholog

Morpholog is an experimental language and runtime for invariant-governed business systems.

Treat this repository as a language/runtime design project, not a conventional CRUD application, ORM, rules engine, or web framework.

## Core ontology

The conceptual core is intentionally tiny:

- **Invariant** — defines admissible governed state.
- **Transformation** — the only lawful way governed state may change.
- **Claim** — runtime substrate; an admitted assertion candidate, not objective truth.

Preserve this ontology unless a change clearly earns an extension. The canonical statement of the semantic model is `docs/runtime-semantics.md`.

## Design principles

- Prefer the smallest semantic step that teaches something concrete.
- Push complexity into the runtime/compiler, not onto users.
- Avoid premature concepts: entities, services, repositories, object models, workflows, metadata systems, parser syntax, projections, or read-side frameworks unless the current task explicitly requires them.
- Treat **"claims about claims"** as the first modelling move for standing, currentness, lineage, and authority. Use claim metadata only when claims cannot express the concept cleanly.
- Never introduce `skip_validation`, `force`, or bypass flags. Exceptions, when eventually needed, must be first-class typed claims carrying reason, approver, scope, and audit trail.
- Keep examples semantically meaningful. Prefer examples that stress admissibility, correction, supersession, current pointers, aggregation, exclusion, or candidate-state rejection.
- Be sceptical: the highest-value review is one that catches concept creep early.

## Current semantic commitments (locked)

- Reads are **snapshot-based** — no read-your-writes inside a transformation.
- Writes are staged as asserted/retracted claims and emitted intents.
- Invariants evaluate against the **candidate post-state**, not the snapshot.
- Rejected transformations must not change governed state.
- The atomic guarantee stops at the database commit. Post-commit outbox intents deliver at-least-once with deterministic idempotency keys; external effects are retried or compensated, never "rolled back."
- Historical claims may remain admitted while current-standing pointer claims move (see `examples/02_revenue_restatement`).
- Financial values are decimal-first via `rust_decimal`. No floating point in business arithmetic.
- Subjects are opaque `uuid::Uuid::now_v7()`. No types over subjects in the surface language.

## Worked examples

The two examples are the test of whether the ontology survives real pressure:

- `examples/01_settlement_netting` — clean kernel proof: existence, equality-via-aggregation, exclusion.
- `examples/02_revenue_restatement` — temporal correction without claim metadata, using a separate `CurrentBankRecognition` pointer claim and `Supersedes` lineage claim.

When reviewing changes to the IR, evaluator, or `propose()` runtime, ask whether the existing tests for these examples still pass *and* whether new semantic ground is being added — not just plumbing.

## Review checklist

When reviewing PRs, ask:

- Does this preserve the invariant/transformation core?
- Does this introduce a new concept before it is proven necessary?
- Does the change make invalid state harder or easier to admit?
- Are rejected paths tested, not just happy paths? Does at least one test prove an invariant *would* reject a candidate state?
- Is terminology precise? `Claim`, not `Fact`. `Transformation`, not service/action/mutation.
- Does the implementation stay small enough to support rapid learning?

Support fast iteration, but protect semantic clarity. Do not request broad architecture, polish, or extra subsystems unless they are necessary for the current milestone.

## Validation commands

For Rust changes, the working set is:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
```

CI runs these exact three commands (`.github/workflows/ci.yml`). If they pass locally, CI should pass.

If PostgreSQL is involved (sqlx integration is the next milestone, not yet present), assume local system PostgreSQL may be used during development unless the task states otherwise.
