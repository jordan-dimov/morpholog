# Refactoring playbook

How to make a type change ripple through the codebase safely when it touches
dozens of sites. Learned from the `Subject`, `actor`, and `Var` slices.

## The principle

In Rust the compiler *guarantees completeness*: change a type at its
definition and `cargo check` enumerates every site that must change - you
cannot miss one. So the risk is never coverage. It is (a) a wrong edit at one
of many mechanical sites, and (b) a big-bang diff no one can review with
confidence. Both are avoidable.

## The shape: expand, migrate, contract

Keep the tree green and committable at every step.

1. **Expand.** Introduce the new type with whatever temporary conversions keep
   the old call sites compiling (`From`, sometimes a temporary `Deref` /
   `AsRef`). Commit.
2. **Migrate.** Flip the type at its definition. The compiler now prints the
   exhaustive worklist. Rewrite the sites guided by that worklist (below) until
   `cargo check` is clean. Commit.
3. **Contract.** Remove the temporary leniency from step 1, forcing the last
   lenient sites to be explicit. Commit.

Each step is a separate, green, bisectable commit. If something breaks later,
`git bisect` lands on the exact step.

For an *opaque* newtype (no `Deref` / `AsRef` - the discipline here) there is
usually nothing to make temporarily lenient, so expand (add the type) and
migrate (flip and propagate) are the whole dance: `Subject`, `actor`, and `Var`
each skipped the contract step.

## Clear the worklist: the compiler is the spine, ast-grep an accelerator

After the type flip, `cargo check --message-format=short` is the spine: it prints
the exhaustive worklist and re-checks every fix, so completeness *and* correctness
are compiler-enforced. How you clear it depends on the worklist's shape:

- **Syntactically uniform** - every site is literally the same transform (e.g.
  `EvalValue::Subject($A)` -> `EvalValue::Subject(Subject::from($A))`). Reach for
  a structural rewrite: one rule does the lot and the diff is trivially reviewable
  ("every site is the same transform").
- **Type-driven** - the right fix differs per site (`.into()` here, `.as_str()`
  there, depending on each site's type). The compiler worklist *is* the tool; walk
  it. A structural rewrite cannot express "do whatever the type needs." The `Var`
  slice was mostly this - do not force ast-grep onto it.

**ast-grep** (`cargo install ast-grep`) is the structural rewriter: syntactic
(tree-sitter), no project model / path resolution / dev-dep-cycle issues, and
**dry-run by default** (writes only with `-U`).

```bash
ast-grep run -p 'EvalValue::Subject($A)' \
  --rewrite 'EvalValue::Subject(Subject::from($A))' -l rust crates/   # preview
# Review the uniform diff, then re-run with -U.
```

Two ast-grep limits, learned the hard way: it **cannot see inside macro bodies**
(`vec!`, `assert_eq!`, `prop_oneof!` are opaque token trees to tree-sitter), so
constructions there need a text pass (`perl -pi`) with the compiler as the net;
and it cannot match by *type*. **rust-analyzer SSR** (`rustup component add
rust-analyzer`; `rust-analyzer ssr 'pat ==>> repl'`) is the type-aware
alternative, but here its pattern paths must be fully resolvable and it warns on
the `morpholog-core` / `morpholog-test-support` dev-dep cycle - reach for it only
when ast-grep's syntactic match cannot express the rule.

## Isolate the semantically-risky bit

The compiler proves the *types* line up; it does not prove *behaviour* is
preserved. Find the few sites where meaning could change - wire formats,
serialisation, persisted shapes, ordering - and:

- give them their own commit, separate from the mechanical mass, and
- pin the contract with a characterisation test *first*, so the change is
  provably behaviour-preserving.

The `actor` retype's serde codec (`morpholog-core::actor_repr`) is the model:
the newtype enforces "an actor is a subject" *once*, at the deserialisation
boundary, and a round-trip test pins the `{"type":"subject", ...}` shape.
Validation moves to the edge; the interior is check-free.

## Watch the boundary costs - but don't re-open the newtype

A newtype changes how it is *looked up*. When a foreign-typed key that is still
outside the slice (a `String`) must query a `HashMap<NewType, _>`, build the
`NewType` keys *once*, not per lookup: the per-lookup `NewType::from(...)` is a
quiet allocation the old borrowed-`&str` lookup did not have (`Var` in
`enumerate_derived` hit exactly this). The tempting "fix" is to give the newtype
`Borrow<str>` or `Deref<Target = str>` so the bare `&str` key works again - do
**not**: that re-opens the string-masquerade the opacity exists to close.
Precompute the keys, or promote the foreign key into the newtype too.

The one ergonomic an opaque id *should* carry is symmetric `PartialEq<str>` /
`PartialEq<&str>`, so `name == "Foo"` and `assert_eq!(name, "Foo")` compile
without `Deref`. Comparison-against-a-literal is where the churn concentrates
(mostly assertions); this kills it while keeping the type opaque to coercion.
It is part of the standard opaque-id surface (`opaque_id!` in
`morpholog-core::ir` generates it alongside `From` / `Display` / `as_str`).

## Where human attention goes

The compiler worklist also tells you where to *look*: once the mechanical sites
are cleared, the residual is a handful of genuine judgement sites (the codec, a
boundary, an ordering). That is where careful review concentrates, not the
mechanical hundred.

## Prefer compiler-driven migration; delegate only uniform rewrites

The `Subject` and `actor` slices were delegated to a sub-agent that hand-edited
~135 and ~50 sites - large, unreviewable diffs. The `Var` slice was done solo
with the compiler worklist and a couple of ast-grep / `perl` passes, and it was
both safer and cheaper. So: prefer doing the migration yourself, leaning on the
compiler. If you do delegate, have the agent *drive these tools* - preview
ast-grep rules, run the compiler-worklist loop, isolate the risky commit - never
hand-edit a hundred sites by judgement.
