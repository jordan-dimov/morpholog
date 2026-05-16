---
applyTo: "**/*.rs"
---

# Rust Review Instructions for Morpholog

Apply these in addition to the repo-wide `.github/copilot-instructions.md`. These trigger on Rust source files.

## Rust idioms (general)

- Prefer `?` for error propagation over `match err { Err(e) => return Err(e), ... }`.
- Reject new `unwrap()` / `expect()` in non-test code. The workspace warns on `clippy::unwrap_used` and `clippy::expect_used`; in tests, the `tests` module already has an `#[allow]` for both.
- Prefer `&str` parameters over `String` unless the function genuinely needs ownership.
- Prefer iterator chains over manual `for` loops when readability is equal or better.
- Use `let ... else { ... };` for early-return patterns instead of nested `if let`.
- Avoid `.clone()` when borrowing or moving would work; flag clones inside evaluator hot paths.
- Avoid `Box<dyn Trait>` unless the trait is genuinely heterogeneous; prefer generics or enums.
- Avoid `unsafe` — the workspace forbids it at the lint level; any introduction is a structural change requiring justification.
- Public API items should have a short doc comment that says *what they do for the caller*, not *how they are implemented*.

## Edition 2024 / Rust 1.95

- `let-else` is idiomatic; use it.
- `cfg_select!` (stable in 1.95) replaces most `cfg-if` uses.
- Edition 2024 lifetime capture rules mean explicit `+ '_` on `impl Trait` is usually unnecessary.

## Morpholog-specific patterns

- Treat IR types (`Invariant`, `Transformation`, `Stmt`, `Expr`, `Term`, `Value`, `Claim`, `Intent`) as a narrow API surface. New variants should be justified by a worked example; new fields on existing variants need stronger justification.
- `eval_invariant` and `propose()` are load-bearing entry points. Changes to their semantics should be exercised against the existing two examples in `crates/morpholog-core/src/lib.rs#tests`, not just isolated unit tests.
- The split between `find_matches` (predicate-side, returns binding extensions) and `eval_value` (value-producing) is deliberate. Don't collapse them — the asymmetry preserves the distinction between "is this true here?" and "what does this evaluate to?".
- `unify_args` is the single matching point against grounded claim args. Changes ripple through every transformation.
- Set semantics on claims is enforced by deduplication in `build_candidate_state`. Don't reintroduce multiset behaviour.
- Idempotency contracts:
  - `Stmt::Assert(c)` is a no-op if `c` is already present in pre-state.
  - `Stmt::Retract { ... }` is a no-op if zero claims match the pattern.

## Test patterns

- Test names read as behavioural specs: `propose_rejects_when_line_already_netted`, not `test_propose_3`.
- Each new transformation should have at least one happy-path test and at least one rejection test — either via `require` failure or an invariant violation on the candidate state.
- Prefer chaining `propose()` calls (`must_accept` helper) over hand-constructing `State { claims: vec![...] }` when feasible — chaining exercises the full loop and matches how state will be populated in production.
- Reuse the `subj()`, `dec()`, `claim_instance()`, and `has_claim()` helpers for consistency.

## What to flag in review

- Any new `unwrap()` or `expect()` outside `#[cfg(test)]`.
- New trait objects (`Box<dyn ...>`) without justification.
- New `clone()` calls in `find_matches`, `find_conjunction`, `unify_args`, or `find_claim_matches`.
- New IR variants without an accompanying test that exercises them.
- New crates added to the workspace without a clear semantic justification.
- Renaming load-bearing types (`Claim`, `Invariant`, `Transformation`) or load-bearing helpers (`propose`, `eval_invariant`) — these are the public ontology and shouldn't drift.
- Changes to `Stmt::Retract` or `Stmt::Assert` semantics — these are the runtime atoms.
- New `panic!()` / `todo!()` / `unimplemented!()` outside `#[cfg(test)]`.

## What not to nag about

- Style choices that `cargo fmt` already handles.
- Lints that `clippy` already catches.
- Missing doc comments on private helpers.
- Use of `Vec<T>` over a more specialised collection (`BTreeSet`, `HashSet`, etc.) in non-hot code — the project is small enough that linear scans are fine until profiling says otherwise.
- Minor naming preferences — focus on terminology *consistency* (Claim, not Fact), not bikeshedding.
