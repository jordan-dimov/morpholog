# Embedding Morpholog

A Morpholog programme is a `.morph` file. The kernel reads it, validates it, runs transformations against it, and explains its rejections. Everything stable an external system needs to drive that loop - input contract, request shape, response envelope - is reachable through the `morpholog` CLI. No Rust toolchain, no FFI, no generated client. A subprocess and JSON.

This document is the public contract. What it pins is what an embedder can rely on; what it deliberately leaves open is reserved for later, when a worked example forces the shape.

## The integration arc

`.morph` is the truth. The schema, the request, and the response are projections of it.

```
                .morph file
                    |
                    v
        +-----------+-----------+
        |                       |
        v                       v
   morpholog schema       morpholog run / explain
   (input contract)       (commit / diagnose)
        |                       |
        v                       v
  embedder validates       embedder parses
  / generates forms        the JSON envelope
```

The embedder asks `morpholog schema <file> <transformation>` for the JSON Schema of the transformation's argument object. For the transformations whose parameters all resolve to unambiguous scalar kinds (`Subject`, `Decimal`, `Date`, `Bool`), it sends that shape to `morpholog run --args-named` to commit (or to `morpholog explain --args-named --json` to dry-run and diagnose). For the corner cases the named codec cannot decode unambiguously - `Polymorphic`, `Unconstrained`, `Ambiguous`, and `Collection` parameters - it falls back to `--args` with the tagged `EvalValue` codec. The named codec's refusals are documented below alongside each pointer to `--args`. Between transitions, `morpholog inspect claims --predicate` reads governed state back, so the next transition can be built from claims the embedder did not itself mint. The embedder parses the JSON envelope on stdout. None of that requires Rust. The `.morph` source is the single source of truth; everything else is derivable from it.

## The argument codecs

`run` and `explain` accept arguments in one of two codecs. Exactly one of `--args` or `--args-named` must be supplied; Clap enforces this at parse time.

### `--args-named` (embedder-facing)

A JSON object keyed by parameter name with **bare** values matching the schema:

```bash
morpholog run trade_lifecycle.morph capture_trade \
    --actor 018f-... \
    --args-named '{
        "trade":"018f-...","commodity":"oil",
        "direction":"buy","version_id":"v1",
        "quantity":"100.5","delivery_period":"2026Q3",
        "captured_on":"2026-05-29","price":"45.20"
    }'
```

Accepts the subset of transformations whose every parameter resolves to one of `Subject`, `Decimal`, `Date`, or `Bool`. Where the schema cannot give an unambiguous scalar kind, the codec refuses with an error pointing at `--args`. Per-kind behaviour:

- **`Subject`** - any JSON string. `Subject` is Morpholog's only primitive noun and carries both minted entity identifiers and domain symbols (commodity codes, period names, account codes, direction enums); the codec mirrors the kernel's opaque-subject model and does not enforce a format. Subjects minted by `Stmt::LetNewSubject` are UUIDv7 by runtime convention; externally supplied Subjects can be anything. Embedders that want UUID validation for a specific parameter layer their own constraint on top in their pre-flight schema.
- **`Decimal`** - JSON string validated against `^-?(0|[1-9]\d*)(\.\d+)?$` (the same pattern the schema emits). Leading `+`, leading zeros, trailing dot, scientific notation: rejected. The CLI matches the schema exactly.
- **`Date`** - JSON string parsed as an ISO-8601 civil date (`YYYY-MM-DD`).
- **`Bool`** - JSON boolean.
- **`Polymorphic`** / **`Unconstrained`** / **`Ambiguous`** / **`Collection`** - refused. The schema either does not give a single unambiguous kind (the first three) or gives a shape the named codec cannot decode without per-item information that v0 does not track (the fourth). Use `--args` with the tagged `EvalValue` codec for these parameters.

Strict beyond the kind check. Missing required keys, unknown keys, wrong JSON types (`true` where a Decimal is expected, etc.), and `null` values are all rejected before any database work. Each error names the parameter, the expected kind, the actual shape, and ends with a pointer at the schema subcommand so the embedder can inspect the accepted shape without leaving the terminal.

### `--args` (implementer-facing)

A JSON array of adjacently-tagged `EvalValue`s, exactly the wire shape the kernel uses internally:

```bash
morpholog run trade_lifecycle.morph capture_trade \
    --actor 018f-... \
    --args '[
        {"type":"subject","value":"018f-..."},
        {"type":"decimal","value":"100.5"}
    ]'
```

The full codec, faithful and unambiguous. Use this when the schema cannot narrow a parameter's kind (the `--args-named` refusals above), when sending Collection values, or when the embedder genuinely wants the lower-level codec.

Both codecs decode through one shared function so the `run` and `explain` paths cannot drift on what counts as a valid input.

## The output envelopes

### `morpholog schema`

Output is a JSON Schema (Draft 2020-12) for the transformation's argument object. No `--json` flag - the output is JSON by definition. The schema is what the embedder validates request bodies against, generates form fields from, or derives typed client models off.

`morpholog schema <file> --intent <IntentType>` emits the same shape for an emitted intent's **payload** instead of a transformation's arguments - the contract a deliverer uses to decode an outbox row (or a `run` outcome's `emitted_intents`) by name rather than by hand-coded position. Intent arguments are declared with explicit kinds, so the payload schema is a direct projection of the declaration. Exactly one of a transformation name or `--intent <Type>` is supplied. Unknown intent exits non-zero with the empty-stdout / `error:` on stderr contract.

**Positional order: `x-morpholog-arg-order`.** Both the transformation-argument and intent-payload schemas carry an `x-morpholog-arg-order` extension: a JSON array of the parameter / field names in declaration order. This is the contract for anything positional - decoding a tagged intent payload array, or building the tagged `--args` codec. `required` happens to list the same names, but it is the JSON Schema validation keyword (semantically a set), so a consumer that needs the order reads `x-morpholog-arg-order`, never the incidental array order of `required`.

Shape:

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "capture_trade",
  "type": "object",
  "additionalProperties": false,
  "required": ["trade", "commodity", ...],
  "x-morpholog-arg-order": ["trade", "commodity", ...],
  "properties": {
    "trade": { "type": "string", "format": "uuid", "description": "..." },
    ...
  }
}
```

Per-kind property fragments are documented in `morpholog-core::schema`'s rustdoc; the same fragments are reused as `anyOf` alternatives for `Ambiguous` parameters.

A note on `Subject`: the schema describes `Subject` as `{"type": "string"}` with no `format`, mirroring the kernel's opaque-subject model. `Subject` is Morpholog's only primitive noun and naturally carries both minted entity identifiers and domain symbols - account codes, period names, commodity codes, direction enums. Subjects minted by `Stmt::LetNewSubject` are UUIDv7 by runtime convention; externally supplied Subjects are opaque strings whose format the kernel does not check. An embedder that wants UUID validation for a specific parameter layers its own constraint on top in its pre-flight schema; the contract Morpholog exposes is the opaque-string shape.

Exits zero on success; non-zero on parse, validation, or unknown-transformation. The schema output stream is empty on any error path.

### `morpholog run`

Without `--trace`, stdout is the `PgProposalOutcome` JSON directly:

```json
{
  "status": "committed",
  "transition_id": "0192-...",
  "actor": {"type":"subject","value":"..."},
  "asserted_claims": [...],
  "retracted_claims": [...],
  "emitted_intents": [...]
}
```

or:

```json
{ "status": "rejected", "reason": "..." }
```

With `--trace`, the envelope wraps the outcome and adds the per-statement trace:

```json
{ "result": <PgProposalOutcome>, "trace": [...] }
```

A kernel error under `--trace` (a transformation that raised `EvalError` mid-execution) carries an `errored` shape inside `result`:

```json
{
  "result": { "status": "errored", "error": "..." },
  "trace": [...]
}
```

The traced and untraced envelopes are intentionally asymmetric; the embedder should decide at request time which it wants, not auto-discriminate.

Exit codes: `0` on a committed outcome; `1` on a rejected outcome or any operational failure (parse, validation, unknown transformation, decoder error, database error).

### `morpholog explain --json`

Stdout is the `Explanation` JSON: the verdict (admissible or rejected), the gate that failed, the directly-missing claims, or the violated invariant. Without `--json` the same structure renders as claim-shaped prose.

Read-only. Exit code is always zero on a parsed-and-validated programme, whether the verdict is admissible or rejected; explaining is answering a question, not taking an action. Only operational failures exit non-zero.

### `morpholog hash`

A stable content hash of the programme's rules: SHA-256 over the *canonical source* - the formatter's rendering of the parsed programme - emitted as `{"program": "<name>", "hash": "sha256:<hex>"}`. Because the formatter/parser round-trip makes that rendering canonical, formatting-only edits do not change the hash; and because comments do not survive canonicalisation, this is **rules-identity, not file-identity** - editing teaching prose leaves the hash alone, editing a rule does not. Record it as a `ruleset_version` in deployment metadata, generated-code headers, and evidence packs ("built against model hash X"). Only a valid programme hashes; parse or validation failures exit non-zero.

### `morpholog schema --all`

The whole contract in one artefact: `{"program", "hash", "predicates", "transformations", "intents"}` - every transformation's argument schema and every intent's payload schema keyed by name, the declared predicate vocabulary (the same shape `inspect predicates` emits, for decoding claim args by field name), and the canonical model hash from `morpholog hash`. Entries appear in declaration order, so the manifest is stable for CI drift-checking; one build-step call replaces N subprocess invocations and N artefacts. The single-shot forms (`schema <transformation>`, `schema --intent <Type>`) are unchanged and mutually exclusive with `--all`.

### `morpholog inspect claims --predicate`

The targeted read of governed state, for building a transition's arguments from claims the embedder did not itself mint (the in-force pointer after a correction, say). `--predicate <Name>` repeats; the result is only claims of the named predicates. Composes with `--as-of` for the historical equivalent - a `transition_id`, or an RFC 3339 timestamp resolved to the last transition committed at or before it - where it scopes the replay itself rather than filtering afterwards.

Stdout is a JSON array of claim objects, each `{"predicate": "<Name>", "args": [<tagged values>]}` - the same tagged-value encoding as the `--args` codec and intent payloads. The args are positional; to decode them by field name, read the predicate's declared argument order from `morpholog inspect predicates <file>` (the read-side analogue of `x-morpholog-arg-order` - never hard-code positions). An unknown predicate name yields an empty array, not an error: the claims table is the authority, not any one programme's vocabulary, so a typo is indistinguishable from a true zero by design.

Selection stops at predicate granularity. Picking one subject's claims out of the result is the embedder's own filtering, and a predicate read returns zero or more claims - multiplicity is the caller's to handle, except where the programme's own invariants pin it (a singleton in-force pointer, say), which is exactly what licenses a simple lookup. Argument-level selection is deliberately left open below.

## Stability and what is not pinned

What this document promises:

- The argument codecs (`--args`, `--args-named`).
- The `morpholog schema` output shape, for both transformation arguments and (`--intent`) intent payloads.
- The `morpholog run` outcome shape, traced and untraced.
- The `morpholog explain --json` Explanation shape.
- The `morpholog inspect claims --predicate` claim-object shape (predicate name plus tagged positional args).
- The `morpholog hash` output shape and its rules-identity semantics (canonical-source SHA-256; formatting and comments excluded).
- The `morpholog schema --all` manifest shape (program, hash, predicates, transformations, intents; declaration order).
- Exit-code semantics for `run` and `explain`.

What is deliberately left open, pending the worked example that forces the shape:

- **Argument-level claim selection.** `inspect claims --predicate` (forced by the `examples/etrm_embedder/` worked embedder) reads claims back at predicate granularity; picking one subject's claims out of the result is client-side. A `--where trade=t1`-style filter waits for an example with a book big enough that the predicate-level cut is not enough.
- **The `--trace` structure internals.** The traced envelope's shape is pinned (`{result, trace}`); the trace entries themselves are richer than the embedder minimum and reserved for the tooling that needs them.
- **The `morpholog inspect` output shapes.** Varied across the inspect subcommands; their own contract document, when one is forced.
- **Result schema generation (a `morpholog schema --result` mode).** The outcome envelope is uniform across transformations, so a documented spec covers it. Auto-generation waits for a real consumer that needs to discriminate dynamically.

The discipline is the same as the rest of Morpholog: ship the contract that an example forces, leave the rest open.
