# The computational model

Status: design doctrine. Companion to [`scope-and-ambition.md`](scope-and-ambition.md), whose algebra this document makes computational: a programme, the admitted state and a proposal give a decision.

Morpholog gives up general computation on purpose. A decision about what may become true should be finite, inspectable and reproducible, and a language that can loop, call out or consult the clock cannot promise any of the three. So the language has no `while`, no recursion, no host functions and no ambient clock. Applications decide what they want to do; Morpholog decides what they are allowed to make true.

The intended headline is **validated Morpholog is computationally total for governed state transitions**. The word *validated* is load-bearing today, and the [termination section](#termination) says exactly why.

This document states eight guarantees. For each it names what makes it true today, what checks it, and where the statement is stronger than the code. A guarantee is not made true by prose: where nothing checks it, it says so and points at the issue that would.

## The eight guarantees

### 1. Termination

A decision over a validated programme, finite admitted state and a finite proposal takes finitely many steps.

- **Holds by:** definitions call each other acyclically; nesting is bounded; everything evaluation ranges over is finite; a transformation body reads the state before it, never its own writes. The argument is [below](#termination).
- **Checked by:** `crates/morpholog-examples/tests/definitions.rs` (cycles refused), `crates/morpholog-core/tests/definitions_adversarial.rs` and `crates/morpholog-core/tests/check_properties.rs` (depth refused, not overflowed), `crates/morpholog-core/src/check/tests.rs` (no rule or derived claim reads a derived claim), `crates/morpholog-examples/tests/computational_model.rs` (every construct names what it ranges over; a body never reads what it admits).
- **Gap:** reaching a validated programme is not yet safe everywhere ([#449](https://github.com/jordan-dimov/morpholog/issues/449), [#450](https://github.com/jordan-dimov/morpholog/issues/450)); a deeply nested value can exhaust the host stack before the steps finish (see [the implementation](#the-statement)).

### 2. Deterministic decisions

The same programme, admitted state, proposal and subject input give the same decision: the same admitted change and intents, or the same rejection (rule, version, witness), or the same error, and the same trace.

- **Holds by:** the kernel takes fresh subjects as an input ([runtime semantics](runtime-semantics.md#fresh-subjects-an-input-not-a-side-effect)), in a fixed draw order; evaluation visits claims in the order of the state it is given, and the proposal loader orders claims by content, so a refusal names the same first violating match between runs.
- **Checked by:** `crates/morpholog-examples/tests/subject_source.rs` (draw order), `crates/morpholog-examples/tests/trace_differential.rs` and `crates/morpholog-postgres/src/scope_differential.rs` (executions compared exactly, with differently seeded hash maps).
- **Gap:** explanations are under no differential ([#452](https://github.com/jordan-dimov/morpholog/issues/452)).

### 3. Pure decisions

Evaluating a rule reads no file, network, process, environment or clock, runs no SQL of its own and calls back into no host code a rule can choose. External computation produces proposals and claims; it never runs inside a decision. Fresh subjects are the one input taken during execution, through one explicit boundary.

- **Holds by:** `morpholog-core` has no I/O dependency; `new Subject()` draws from a source the caller supplies.
- **Checked by:** `scripts/kernel_purity.sh` (in precommit and CI): no randomness crate among the kernel's dependencies, and no clock, randomness, file, network, process, environment or thread access in its source. A tripwire, not a proof.
- **Gap:** a subject source is caller code, so termination assumes it answers (see premise T6).

### 4. Explicit state

A rule's truth depends only on the programme, the proposal, the state and the subject input it is given. A transformation reads one fixed snapshot of the state before it; its admissions are staged and become visible only to the invariant check that follows, never to the body itself.

- **Holds by:** every body statement evaluates against the pre-state; a subject drawn by `new Subject()` is an input like the proposal's arguments, and running out of subjects is a typed error; `pre(...)` in a body is an error, because the body already reads only the pre-state; a `for` collection is evaluated once, before its body runs.
- **Checked by:** `crates/morpholog-examples/tests/computational_model.rs` (`a_transformation_never_reads_what_it_admits`).

### 5. Replay

A committed decision can be reconstructed from its recorded inputs under its recorded rules, and its meaning never depends on today's clock, mutable host code or a remote service.

- **Holds by:** the audit record carries the proposal's arguments, the attested actor and the hash of the whole programme that admitted it, inside the tamper-evident tree.
- **Gap:** the record does not hold the subjects a decision drew, so exact re-execution needs them supplied ([#403](https://github.com/jordan-dimov/morpholog/issues/403)); nothing records which version of the kernel's semantics decided ([#451](https://github.com/jordan-dimov/morpholog/issues/451)).

### 6. Semantic identity

The programme hash names what the rules mean, not how they run: plans, indexes, compilation and deployment never become part of it.

- **Holds by:** the canonical hash covers the programme; a prepared programme and its execution plan are caches ([runtime semantics](runtime-semantics.md#programme-preparation-execution)); generated machinery is left out of the hash only where validation proves it derives from what the hash covers.
- **Gap:** the hash names the rules; nothing yet names the interpreter contract they ran under ([#451](https://github.com/jordan-dimov/morpholog/issues/451)).

### 7. One reference semantics

The kernel is the meaning. The compiled SQL route, mixed execution and any later backend must decide what the kernel decides over the fragment they claim, or refuse the fragment.

- **Holds by:** the compiler refuses what it cannot render faithfully, by a typed reason; an execution layer that takes part of a check on itself hands the rest back through `morpholog_core::execution`, whose operations state what the caller owes.
- **Checked by:** `crates/morpholog-postgres/src/compiled_differential.rs`, `crates/morpholog-postgres/src/scope_differential.rs`, `crates/morpholog-postgres/src/load_differential.rs`.
- **Gap:** the differentials check a corpus, not every programme. They are evidence, not a proof.

### 8. Bounded authority

What a transformation can write and emit is visible in its text: every `admit`, `retract` and `emit` names a declared predicate or intent literally. No rule can gain authority to change arbitrary state or perform an effect.

- **Holds by:** the statement forms carry their target's name, never a computed one; validation refuses an undeclared name and a write to a derived claim; effects leave only as intents, delivered after commit by workers outside the kernel.
- **Checked by:** `crates/morpholog-examples/tests/computational_model.rs` (the construct census), the validation suites.

## Termination

### The statement

**Semantic theorem.** For a validated programme, finite admitted state and finite materialised inputs, with a subject input that answers each request, evaluation takes finitely many steps.

"Total" here means no divergence. A step may end the evaluation with a typed kernel error, which is a terminating outcome; which errors are reachable is a separate question, held by `crates/morpholog-examples/tests/eval_totality.rs`.

**The implementation.** The Rust evaluator carries those steps out and ends in a decision or a typed kernel error, on one condition the language does not yet enforce: the values it is given must fit within the host's stack. Validation caps how deep a programme nests (T2), but nothing caps a value a proposal carries. A collection nested inside collections is copied, compared and ordered recursively, so one nested deep enough could exhaust the stack, which is neither a decision nor a typed error. No typed refusal guards against that yet.

**Establishing the premise.** The theorem starts from a validated programme, so reaching one must itself be safe. The shipped CLI validates every programme before evaluating one, and every proposal path, in the CLI or the PostgreSQL adapter, takes a programme built from a validated `PreparedProgram`. Two gaps remain, both in getting to the premise rather than in the theorem:

- some public functions accept unvalidated programmes directly: the kernel's evaluators, and the adapter's read-side scoring, coverage and derived-claim functions. Cyclic definitions handed to one of them can overflow the stack instead of returning a typed result (shown for `eval_invariant`) ([#449](https://github.com/jordan-dimov/morpholog/issues/449));
- validation itself searches each definition body for calls, recursively, before its depth guard can refuse it, so a definition nested deep enough overflows `validate()` ([#450](https://github.com/jordan-dimov/morpholog/issues/450)).

### The rank

Give every node of a validated programme an evaluation rank:

- a node with nothing beneath it has rank 1;
- a node's rank is one more than the highest rank of the nodes its evaluation recurses into;
- a definition call ranks one above its definition's body;
- an `xor` ranks one above its lowering, `(a or b) and not (a and b)`.

The rank is finite. Definitions call each other acyclically, so ranking them in call order (callees first) never waits on itself, and validation refuses a cycle. The `xor` lowering is larger than the `xor`, because it copies both operands, but its rank is fixed by those operands, which rank lower. Copying changes cost, not termination.

Every recursive step of the evaluator goes to a node of strictly lower rank: proposition to sub-proposition, value to sub-value, call to body, `xor` to its lowering, `for` to its body. Evaluating the same node against another state (`pre(...)`), another definition frame or other bindings recurses into the same lower-ranked nodes.

### Finite work at each node

At each node, any repeated work ranges over something finite and already materialised:

- a claim pattern, a lookup or a `retract` matches against the admitted state, which is finite; the candidate state is the pre-state with finitely many admissions and retractions applied;
- `for` and `in` range over a collection value, which is a finite list;
- `sum`, `min`/`max` over a body, `forall` and `exists` range over a finite set of matches;
- a transformation body is a finite list of statements, and a decision checks a finite list of invariants.

So, by induction on rank, every evaluation is finite. A transformation reaches `new Subject()` finitely often, so it consumes a finite prefix of the subject input.

A few loops iterate no finite container, and each carries its own ranking argument:

- `period_index` searches by halving: its bounds start a fixed distance apart (twice the number of days in the calendar, plus four) and the gap halves each step;
- an exact sum's total sheds one trailing zero per step, and its scale strictly falls;
- a calendar span's text is consumed as it is parsed.

### The premises

| | Premise | Pinned by |
|---|---|---|
| T1 | Definitions call each other acyclically. | `DefinitionCycle`; `crates/morpholog-examples/tests/definitions.rs` |
| T2 | Nesting, counting a definition call at its body's depth, stays within a fixed limit. This bounds the evaluator's call stack. | `NestingTooDeep`; `crates/morpholog-core/tests/check_properties.rs`, `crates/morpholog-core/tests/definitions_adversarial.rs` |
| T3 | No construct ranges over anything but the finite state, a finite collection, its own children, a definition's body or the subject input, apart from the self-bounded loops above. | the construct census, `crates/morpholog-examples/tests/computational_model.rs` |
| T4 | Derived claims form one layer: a derived claim is computed from admitted claims only, and no rule reads one. There are no recursive views. | `crates/morpholog-core/src/check/tests.rs` |
| T5 | A body never reads its own staged writes, so nothing feeds back into the state it reads. | `a_transformation_never_reads_what_it_admits` |
| T6 | The subject input answers each request, with a subject or with nothing left, in finite time. A subject source is caller code. | the `SubjectSource` contract |

The compiled route runs SQL the compiler writes, which is never recursive, over finite tables. That it decides what the kernel decides is guarantee 7.

### What this does not claim

- **Cost.** Termination is not speed. A join over several claim patterns can cost the size of the state raised to the number of patterns. Case-local admission and the compiled route exist for cost, and change it without changing meaning.
- **Parsing.** Parsing a `.morph` file precedes validation and is not a decision. The parser has no depth guard, and its time grows faster than linearly with deeply nested parentheses (see the roadmap).

## Reviewing a change

Every change answers the algebra's question first: does it introduce a new semantic concept, or another way of preparing, proposing, deciding, reading or proving?

**A strategy change** (a planner, an index, a compiled route, transport, caching, packaging of evidence) answers one question: are the eight guarantees and the observable outcome unchanged? If so, it proves that at the seam it replaces, with a differential or a parity test, and earns no new concept.

**A semantic change** (a new construct, a new builtin, a change to what a construct means) carries a short computational doctrine note in its issue or design note, answering each of the eight guarantees with one of:

- **preserved:** why the change stays inside the guarantee;
- **strengthened:** the change removes a gap or makes the guarantee more mechanical;
- **changed:** the guarantee itself would move, which needs a separate amendment to this document first;
- **unknown:** stop at design until it is known.

It also names the proof it relies on, in the cheapest honest form: a static refusal, a structural argument, the construct census, a differential, or a break-checked test. A new construct joins the census in the same change, naming what it ranges over.

The forcing-example rule still holds: a new construct needs a worked case. A forcing example can justify a feature. It cannot quietly weaken a guarantee.

The questions this is for: a `while`, a recursive `define`, a host function, a clock, a random value, an external lookup, a new optimiser or execution route. Each of them moves at least one guarantee, and the review should say which before code lands.
