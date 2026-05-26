# Refactoring playbook

How to make a type change ripple through the codebase safely when it touches
dozens of sites. Learned the hard way from the `Subject` newtype and the
`actor` retype.

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
   exhaustive worklist. Rewrite the sites - mechanically, with a structural
   tool, not by hand (below) - until `cargo check` is clean. Commit.
3. **Contract.** Remove the temporary leniency from step 1, forcing the last
   lenient sites to be explicit. Commit.

Each step is a separate, green, bisectable commit. If something breaks later,
`git bisect` lands on the exact step.

## Do the mechanical mass with a structural tool, not by hand

N hand-edits is N chances to get one subtly wrong. A structural rewrite applies
*one rule* uniformly, and the diff is trivially reviewable - every site is the
same transform.

**ast-grep** (`cargo install ast-grep`) is the primary tool. It is syntactic
(tree-sitter): no project model, no path resolution, no dev-dep-cycle issues,
and **dry-run by default** - it only writes with `-U`.

```bash
# Preview (writes nothing):
ast-grep run -p 'EvalValue::Subject($A)' \
  --rewrite 'EvalValue::Subject(Subject::from($A))' -l rust crates/
# Review the uniform diff, then apply with -U.
```

**rust-analyzer SSR** (`rustup component add rust-analyzer`;
`rust-analyzer ssr 'pat ==>> repl'`) is the type-aware alternative: it matches
by resolved item identity and can tell, say, a `.clone()` on a `String` from one
on another type. Two caveats here: pattern paths must be fully resolvable from
the invocation context, and it warns on the `morpholog-core` /
`morpholog-test-support` dev-dependency cycle. Reach for it only when
ast-grep's syntactic matching cannot express the rule.

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

## Where human attention goes

After the structural rules run, `cargo check --message-format=short` lists the
residual - usually a handful of genuine judgement sites (the codec, a boundary,
an ordering). That is where careful review concentrates, not the mechanical
hundred.

## If delegating to a sub-agent

Have it *drive these tools* - write and preview ast-grep rules, run the
compiler-worklist loop, isolate the risky commit - not hand-edit a hundred
sites. Delegation plus determinism; never delegation plus a hundred judgements.
