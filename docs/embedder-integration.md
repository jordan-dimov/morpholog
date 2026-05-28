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

The embedder asks `morpholog schema <file> <transformation>` for the JSON Schema of the transformation's argument object. It sends that shape to `morpholog run --args-named` to commit (or to `morpholog explain --args-named --json` to dry-run and diagnose). It parses the JSON envelope on stdout. None of that requires Rust. The `.morph` source is the single source of truth; everything else is derivable from it.

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

Strict by design. Missing required keys, unknown keys, wrong JSON types, and `null` values are all rejected before any database work. Each error names the parameter, the expected kind, the actual shape, and ends with a pointer at the schema subcommand so the embedder can inspect the accepted shape without leaving the terminal.

Refuses what it cannot decode unambiguously. `Polymorphic`, `Unconstrained`, `Ambiguous`, and `Collection` parameters all error with a pointer at `--args` as the fallback. The schema gives the kind or it does not; the codec does not guess from JSON shape (strings that look like UUIDs are not implicitly Subject).

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

Shape:

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "capture_trade",
  "type": "object",
  "additionalProperties": false,
  "required": ["trade", "commodity", ...],
  "properties": {
    "trade": { "type": "string", "format": "uuid", "description": "..." },
    ...
  }
}
```

Per-kind property fragments are documented in `morpholog-core::schema`'s rustdoc; the same fragments are reused as `anyOf` alternatives for `Ambiguous` parameters.

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

## Stability and what is not pinned

What this document promises:

- The argument codecs (`--args`, `--args-named`).
- The `morpholog schema` output shape.
- The `morpholog run` outcome shape, traced and untraced.
- The `morpholog explain --json` Explanation shape.
- Exit-code semantics for `run` and `explain`.

What is deliberately left open, pending the worked example that forces the shape:

- **Intent payload schemas.** Each declared intent has an argument vocabulary; the schema subcommand could in principle emit them too. Reserved for the embedder that consumes intent payloads.
- **The `--trace` structure internals.** The traced envelope's shape is pinned (`{result, trace}`); the trace entries themselves are richer than the embedder minimum and reserved for the tooling that needs them.
- **The `morpholog inspect` output shapes.** Varied across the inspect subcommands; their own contract document, when one is forced.
- **Result schema generation (a `morpholog schema --result` mode).** The outcome envelope is uniform across transformations, so a documented spec covers it. Auto-generation waits for a real consumer that needs to discriminate dynamically.

The discipline is the same as the rest of Morpholog: ship the contract that an example forces, leave the rest open.
