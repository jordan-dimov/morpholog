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
   morpholog schema       morpholog propose / explain
   (input contract)       (commit / diagnose)
        |                       |
        v                       v
  embedder validates       embedder parses
  / generates forms        the JSON envelope
```

The embedder asks `morpholog schema <file> <transformation>` for the JSON Schema of the transformation's argument object. For the transformations whose parameters all resolve to unambiguous scalar kinds (`Subject`, `Decimal`, `Decimal[U]`, `Date`, `Timestamp`, `Duration`, `Bool`), it sends that shape to `morpholog propose --args-named` to commit (or to `morpholog explain --args-named --json` to dry-run and diagnose). For the corner cases the named codec cannot decode unambiguously - `Polymorphic`, `Unconstrained`, `Ambiguous`, and `Collection` parameters - it falls back to `--args` with the tagged `EvalValue` codec. The named codec's refusals are documented below alongside each pointer to `--args`. Between transitions, `morpholog inspect claims --predicate` reads governed state back - decoded by declared field name under `--named` - so the next transition can be built from claims the embedder did not itself mint. On day zero, `morpholog init` provisions the schema from the binary itself. The embedder parses the JSON envelope on stdout. None of that requires Rust. The `.morph` source is the single source of truth; everything else is derivable from it.

## The argument codecs

`propose` and `explain` accept arguments in one of two codecs. Exactly one of `--args` or `--args-named` must be supplied; Clap enforces this at parse time.

### `--args-named` (embedder-facing)

A JSON object keyed by parameter name with **bare** values matching the schema:

```bash
morpholog propose trade_lifecycle.morph capture_trade \
    --actor 018f-... \
    --args-named '{
        "trade":"018f-...","commodity":"oil",
        "direction":"buy","version_id":"v1",
        "quantity":"100.5","delivery_period":"2026Q3",
        "captured_on":"2026-05-29","price":"45.20"
    }'
```

Accepts the subset of transformations whose every parameter resolves to one of `Subject`, `Decimal`, `Decimal[U]`, `Date`, `Timestamp`, `Duration`, or `Bool`. Where the schema cannot give an unambiguous scalar kind, the codec refuses with an error pointing at `--args`. Per-kind behaviour:

- **`Subject`** - any JSON string. `Subject` is Morpholog's only primitive noun and carries both minted entity identifiers and domain symbols (commodity codes, period names, account codes, direction enums); the codec mirrors the kernel's opaque-subject model and does not enforce a format. Subjects minted by `Stmt::LetNewSubject` are UUIDv7 by runtime convention; externally supplied Subjects can be anything. Embedders that want UUID validation for a specific parameter layer their own constraint on top in their pre-flight schema.
- **`Decimal`** - JSON string validated against `^-?(0|[1-9]\d*)(\.\d+)?$` (the same pattern the schema emits). Leading `+`, leading zeros, trailing dot, scientific notation: rejected. The CLI matches the schema exactly.
- **`Decimal[U]` (unit-tagged)** - the SAME bare decimal string as `Decimal`. The declaration fixes the unit, so the wire never carries it (sending it would create a second source of truth); the schema property adds `x-morpholog-unit` and names the unit in the description. The tagged `--args` codec is the self-describing exception: `{"type":"quantity","value":{"amount":"25000","unit":"USD"}}`.
- **`Date`** - JSON string parsed as an ISO-8601 civil date (`YYYY-MM-DD`).
- **`Timestamp`** - JSON string parsed as an RFC 3339 instant (`2026-10-24T14:00:00Z`). Zone-less UTC by design: local-time interpretation is admitted as claims, never assumed by the runtime.
- **`Duration`** - JSON string parsed as an ISO-8601 duration in exact time units (`PT6H`, `PT1H30M`); calendar units (months, years) are rejected.
- **`Bool`** - JSON boolean.
- **`Polymorphic`** / **`Unconstrained`** / **`Ambiguous`** / **`Collection`** - refused. The schema either does not give a single unambiguous kind (the first three) or gives a shape the named codec cannot decode without per-item information that v0 does not track (the fourth). Use `--args` with the tagged `EvalValue` codec for these parameters.

Strict beyond the kind check. Missing required keys, unknown keys, wrong JSON types (`true` where a Decimal is expected, etc.), and `null` values are all rejected before any database work. Each error names the parameter, the expected kind, the actual shape, and ends with a pointer at the schema subcommand so the embedder can inspect the accepted shape without leaving the terminal.

### `--args` (implementer-facing)

A JSON array of adjacently-tagged `EvalValue`s, exactly the wire shape the kernel uses internally:

```bash
morpholog propose trade_lifecycle.morph capture_trade \
    --actor 018f-... \
    --args '[
        {"type":"subject","value":"018f-..."},
        {"type":"decimal","value":"100.5"}
    ]'
```

The full codec, faithful and unambiguous. Use this when the schema cannot narrow a parameter's kind (the `--args-named` refusals above), when sending Collection values, or when the embedder genuinely wants the lower-level codec.

Both codecs decode through one shared function so the `propose` and `explain` paths cannot drift on what counts as a valid input.

## The output envelopes

### `morpholog schema`

Output is a JSON Schema (Draft 2020-12) for the transformation's argument object. No `--json` flag - the output is JSON by definition. The schema is what the embedder validates request bodies against, generates form fields from, or derives typed client models off.

`morpholog schema <file> --intent <IntentType>` emits the same shape for an emitted intent's **payload** instead of a transformation's arguments - the contract a deliverer uses to decode an outbox row (or a `propose` outcome's `emitted_intents`) by name rather than by hand-coded position. Intent arguments are declared with explicit kinds, so the payload schema is a direct projection of the declaration. Exactly one of a transformation name or `--intent <Type>` is supplied. Unknown intent exits non-zero with the empty-stdout / `error:` on stderr contract.

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

### `morpholog propose`

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

On a single-proposal rejection whose reason names an invariant declared in the source, stderr carries a courtesy line locating the rule (`rule at <file>:<line>:<col> (<name>)`). It is for the human at the terminal, not the integration: parse stdout only. Batch mode never prints it - receipts are the whole contract there.

### `morpholog explain --json`

Stdout is the `Explanation` JSON: the verdict (admissible or rejected), the gate that failed, the directly-missing claims, or the violated invariant. Without `--json` the same structure renders as claim-shaped prose.

Read-only. Exit code is always zero on a parsed-and-validated programme, whether the verdict is admissible or rejected; explaining is answering a question, not taking an action. Only operational failures exit non-zero.

### `morpholog check --json`

The authoring gate's machine-readable shape, for an embedder (or its authoring AI) that wants findings as data rather than rendered carets. Stdout is one object:

```json
{
  "file": "model.morph",
  "diagnostics": [
    {
      "severity": "error",
      "message": "undeclared predicate `Ghost` referenced in invariant `cap`",
      "start": 412, "end": 447,
      "line": 19, "column": 1
    }
  ]
}
```

One entry per finding - parse errors, validation errors, and lints uniformly - with `severity` either `"error"` or `"hint"`. `start`/`end` are byte offsets into the file, `line`/`column` 1-based; a finding with no source anchor (one against a generated discipline invariant, say) carries only `severity` and `message`. A clean programme emits an empty `diagnostics` array. Exit semantics match the plain form: `0` when nothing failed, `1` on any error, and `--strict` promotes hints to errors (in the JSON too). Without `--json`, the same findings render as ariadne caret blocks on stderr and stdout stays script-silent.

### `morpholog init`

Provisions the `morpholog` schema (claims, audit, outbox) in an existing database, from the canonical schema **embedded in the binary** - a binary-only deployment provisions exactly the schema its build expects, with nothing to vendor and nothing to drift. Day-zero only: if the schema already exists it refuses (exit non-zero, the remedy named), or reports `{"status": "already-initialised"}` and exits zero under `--skip-if-exists` - the flag for deployment entrypoints that may re-run. It never drops and never migrates; schema evolution is deliberately out of this command's scope.

### `propose --explain-on-reject`

With the flag, a business rejection's envelope carries an `explanation` field: the same structured account `explain --json` emits, computed against **the exact pre-state the gates evaluated** - one snapshot, not a propose-then-explain pair whose second read can describe different state than the one that refused. Committed envelopes are unchanged (no `explanation` field), exit codes are unchanged, and only business rejections are explained - kernel errors and serialization failures have no admissibility story to tell. Mutually exclusive with `--trace`.

### `morpholog hash`

A stable content hash of the programme's rules: SHA-256 over the *canonical source* - the formatter's rendering of the parsed programme - emitted as `{"program": "<name>", "hash": "sha256:<hex>"}`. Because the formatter/parser round-trip makes that rendering canonical, formatting-only edits do not change the hash; and because comments do not survive canonicalisation, this is **rules-identity, not file-identity** - editing teaching prose leaves the hash alone, editing a rule does not. Record it as a `ruleset_version` in deployment metadata, generated-code headers, and evidence packs ("built against model hash X"). Only a valid programme hashes; parse or validation failures exit non-zero.

### `morpholog schema --all`

The whole contract in one artefact, with top-level keys `program`, `hash`, `predicates`, `transformation_order`, `transformations`, `intent_order`, and `intents`: every transformation's argument schema and every intent's payload schema keyed by name for lookup, the declared predicate vocabulary (the same shape `inspect predicates` emits, for decoding claim args by field name), and the canonical model hash from `morpholog hash`. Declaration order is carried by the explicit `transformation_order` / `intent_order` arrays - the manifest-level analogue of `x-morpholog-arg-order` - because JSON object key order is never a contract; the keyed objects themselves serialise with sorted keys, so the whole artefact is byte-stable for CI drift-checking. One build-step call replaces N subprocess invocations and N artefacts. The single-shot forms (`schema <transformation>`, `schema --intent <Type>`) are unchanged and mutually exclusive with `--all`.

### `morpholog inspect claims --predicate`

The targeted read of governed state, for building a transition's arguments from claims the embedder did not itself mint (the in-force pointer after a correction, say). `--predicate <Name>` repeats; the result is only claims of the named predicates. Composes with `--as-of` for the historical equivalent - a `transition_id`, or an RFC 3339 timestamp resolved to the last transition committed at or before it - where it scopes the replay itself rather than filtering afterwards.

Stdout is a JSON array of claim objects, each `{"predicate": "<Name>", "args": [<tagged values>]}` - the same tagged-value encoding as the `--args` codec and intent payloads. An unknown predicate name yields an empty array, not an error: the claims table is the authority, not any one programme's vocabulary, so a typo is indistinguishable from a true zero by design.

**`--named <file.morph>`** decodes each claim's positional args into a bare named object - `{"predicate": "<Name>", "args": {field: bare_value, ...}}`, the read-side mirror of `--args-named` (same exactness rules: decimals, dates, timestamps, and durations stay strings; booleans are booleans; collections recurse). With `--named` the programme becomes the authority, in both directions: a *requested* `--predicate` the file does not declare is a hard error raised before any database read (the typo the bare read tolerates), and a *returned* claim whose predicate is undeclared, or whose arity disagrees with its declaration, is a **hard error naming both sides** (programme/database skew) - never a silent skip or an empty result. Composes with `--predicate` and `--as-of`. Without `--named`, decoding by hand stays possible via `inspect predicates` and the tagged output above.

Selection stops at predicate granularity. Picking one subject's claims out of the result is the embedder's own filtering, and a predicate read returns zero or more claims - multiplicity is the caller's to handle, except where the programme's own invariants pin it (a singleton in-force pointer, say), which is exactly what licenses a simple lookup. Argument-level selection is deliberately left open below.

### `morpholog inspect audit` - the projector's tail

The blessed read for downstream projectors (forced by the first real one: an ETRM read side folding transitions into blotters and positions). Stdout is NDJSON - one committed transition per line, in `(committed_at, transition_id)` order, each line an `audit_row` exactly as pinned in the `$defs` (transition id, transformation name, tagged arguments and actor, the invariants checked with their versions, asserted/retracted claims, emitted intents, `committed_at`). An empty tail is empty stdout, exit 0 - the poll loop's steady state. There is no `--limit` and no `--follow`: the poll loop is the projector's own.

**`--after <transition_id>`** resumes strictly after a previously seen transition - one opaque token, since every commit envelope, batch receipt, and audit line already carries one. An unknown id is a hard error naming it, never a silent restart from zero.

**The resume is lossless, and the guarantee has a mechanism.** `committed_at` is the WRITER's transaction start time (server-evaluated `now()`), while row visibility follows commit order - so a writer still in flight when a reader snapshots leaves a row that sorts BELOW rows the reader already emitted, and a naive cursor would skip it forever. Each invocation therefore computes a resume horizon first - the minimum transaction start over the database's open transactions - and only then takes its snapshot, emitting nothing at or above the horizon. Rows beyond it are withheld, never lost: the next invocation's fresh horizon surfaces them. Preconditions: the reader's role must see the writers' sessions in `pg_stat_activity` (same role, `pg_read_all_stats`, or superuser - insufficient visibility is detected and is an error naming the remedy, never an unsound frontier), and no prepared transactions write audit (PostgreSQL's default `max_prepared_transactions = 0`). Liveness: the horizon trails the oldest open transaction in the database, so a stuck session stalls the tail without ever losing rows.

**`--named <file.morph>`** decodes each line's asserted/retracted claims into the `named_claim` shape under the programme's authority, with the same hard-error skew contract as the claims read. `arguments` and `emitted_intents` deliberately stay tagged: they belong to the transformation and intent vocabularies (parameter kinds), not predicate declarations, and the asymmetry is stated rather than papered over with a second decoder.

`--as-of` does not apply: the audit table IS the chronological record, and the tail's coordinate is `--after`.

**For direct SQL readers** (a projector that prefers the table): the `(committed_at, transition_id)` total order is stable - it is load-bearing in `verify`, coverage replay, and the as-of reconstructions, so it cannot drift; the claim JSONB inside `asserted_claims`/`retracted_claims` is the pinned `claim_instance` shape; the column set may grow additively. And `committed_at` is the transaction START instant, not the commit instant - any direct tail wanting lossless resume needs the same compute-horizon-first recipe the CLI implements (min `xact_start` over `pg_stat_activity`, then snapshot, then page strictly below it). The recipe carries the CLI's preconditions with it: the tailing session must SEE the writers' sessions in `pg_stat_activity` (same role, `pg_read_all_stats`, or superuser - a hidden session silently falls out of the minimum and the horizon is unsound; the runtime API detects this and errors rather than tailing unsafely, and a hand-rolled tail must do the same), and no prepared transactions may write audit (PostgreSQL's default `max_prepared_transactions = 0`).

## Stability and what is not pinned

What this document promises:

- The argument codecs (`--args`, `--args-named`).
- The `morpholog schema` output shape, for both transformation arguments and (`--intent`) intent payloads.
- The `morpholog propose` outcome shape, traced and untraced (the `$defs` key keeps its historical `run_outcome` name).
- The `morpholog explain --json` Explanation shape.
- The `morpholog inspect claims --predicate` claim-object shape (predicate name plus tagged positional args).
- The `morpholog init` provisioning contract (embedded schema, day-zero only, refuse-or-skip on an existing schema, never drop or migrate).
- The `propose --explain-on-reject` envelope (rejections gain `explanation` in the `explain --json` shape, computed against the rejecting snapshot; commits unchanged).
- The `inspect claims --named` decoded-claim shape and its hard-error skew contract.
- The `inspect audit` NDJSON tail (the `audit_row` / `audit_row_named` line shapes, the `(committed_at, transition_id)` order, `--after`'s lossless-resume semantics and its preconditions).
- The `morpholog hash` output shape and its rules-identity semantics (canonical-source SHA-256; formatting and comments excluded).
- The `morpholog schema --all` manifest shape (program, hash, predicates, transformations, intents; the keyed objects serialise with sorted keys, and declaration order travels in the explicit `transformation_order` / `intent_order` arrays).
- The `morpholog schema --result` outcome-envelope contract (one `$defs` entry per machine-readable envelope).
- Exit-code semantics for `propose` and `explain`.

What is deliberately left open, pending the worked example that forces the shape:

- **Argument-level claim selection.** `inspect claims --predicate` (forced by the `examples/etrm_embedder/` worked embedder) reads claims back at predicate granularity; picking one subject's claims out of the result is client-side. A `--where trade=t1`-style filter waits for an example with a book big enough that the predicate-level cut is not enough.
- **The `--trace` structure internals.** The traced envelope's shape is pinned (`{result, trace}`); the trace entries themselves are richer than the embedder minimum and reserved for the tooling that needs them.
- **The remaining `morpholog inspect` output shapes.** The claims and audit shapes are pinned above; `rejections`, `outbox`, `derived`, and `guarantees` vary and earn their own contract entries when an embedder leans on them. (`inspect rejections` lists the operational rejection log - refusals recorded after rollback, at-most-once; the `propose` envelope itself is unchanged by that log's existence.)
The discipline is the same as the rest of Morpholog: ship the contract that an example forces, leave the rest open.

## The outcome-envelope contract (`schema --result`)

`morpholog schema --result` emits one JSON Schema (Draft 2020-12) document whose `$defs` pin every machine-readable envelope this document describes in prose: the tagged value encoding, claim and intent instances, the `propose` outcome union (the `run_outcome` def: committed | rejected, the latter optionally carrying `--explain-on-reject`'s `explanation`), the traced envelope, the `explain --json` Explanation with its gate / invariant / error verdicts, batch receipts, the audit tail's row shapes (tagged and `--named`), outbox rows and the claim / complete / release wrappers, the `check --json` report, the `inspect coverage --json` report, and the `hash` / `init` reports. Programme-independent - the shapes vary only with the binary, so no `.morph` file is taken and the document is byte-stable for a given build.

This is the artefact client generation consumes (the reserved "waits for a real consumer" slot is now filled): the generated Python client's envelope models derive from the same pinned document, and a contract-test suite in the binary's own repository holds the document, the binary's real serialization, and the generated models to one set of golden envelopes. Trace entry internals remain reserved (the `trace` array is pinned as an array, its items deliberately unconstrained).

## The generated Python client (`generate python-client`)

```bash
morpholog generate python-client <file.morph> --out <dir>
```

emits a complete, self-contained `morpholog_client/` package: the value codecs both directions (decimals as `decimal.Decimal` end to end, aware datetimes with naive ones refused on write), the envelope models (key-set strict - an envelope field this client does not know raises instead of dropping data), the subprocess adapter over the whole surface this document pins (including `propose --batch`, `check --json`, `inspect coverage`, the `inspect audit` tail as `audit()` / `audit_named()` with the `--after` resume cursor, and `as_of` on both claims reads - a transition id or RFC 3339 timestamp, threading the binary's `--as-of`), a frozen request dataclass per transformation with fields in declaration order and `to_args_named()`, a read model per predicate parsing the named read's wire-true values by declared kind, and a payload model per intent with the positional arg order baked in - no runtime `schema --intent` call. `__init__.py` carries the stamps (`PROGRAM`, `MODEL_HASH`, `MORPHOLOG_VERSION`) and enforces the declared Python floor (3.10) at first import.

The properties that make it a contract rather than a convenience: **stdlib-only** (no dependency treadmill; richer types are the embedder's to build on top), **deterministic** (the same binary and programme produce byte-identical output, so the drift check is regenerate-and-diff), and **whole-run refusal** (a programme whose contract the client cannot carry - a Duration field, a parameter with no single concrete kind, a Python-keyword field name - fails generation with every finding listed and nothing written; no partial packages, no silent mangling). The worked embedder (`examples/etrm_embedder/`) runs on its committed output.

## Disciplines on the manifest (additive)

A predicate declaration may carry claim disciplines (`unique by`,
`append only`, `current pointer by`, `superseded via`; see
[`runtime-semantics.md`](runtime-semantics.md)). They appear in
`schema --all` and `inspect predicates` as an additional `disciplines`
array on the predicate object, serialised only when present - a
programme without disciplines produces byte-identical output to
before the field existed, so existing consumers are unaffected. The
field is informative for embedders (the enforcement happens inside
Morpholog, as generated invariants and authoring-time checks); a
client that surfaces it should treat unknown discipline tags as
opaque.

## Batch proposals (`propose --batch`)

`morpholog propose <file.morph> --batch <rows.ndjson>` (`-` reads stdin)
admits many governed transitions in one invocation. One JSON object
per line:

```json
{"transformation": "post_simple_entry", "actor": "jordan", "args_named": {"entry_id": "e1", "...": "..."}}
```

`args` (the tagged codec) may replace `args_named`; exactly one per
row, decoded by the same codecs as the flags. Each row commits or
rolls back on its own - an import is explicitly NOT all-or-nothing -
and produces one NDJSON receipt on stdout in input order: the
single-proposal envelope above plus `"row"`, the 1-based input line number
(blank lines skip silently, so receipts map back to the file). A
malformed row (bad JSON, unknown transformation, undecodable args)
yields `{"row": N, "status": "error", "error": "..."}` and processing
continues. `--explain-on-reject` composes per row, exactly as in
single proposals. A summary line lands on stderr.

**Exit code contract - deliberately different from a single `propose`.**
A single `propose` exits 1 on a rejection; a batch exits 0 whenever every
row was processed, because partial admission is an import's normal
outcome - the receipts are the result, not the exit code. Non-zero is
reserved for operational failure: unreadable input, a programme that
fails validation, a broken connection. A serialization conflict
(SQLSTATE 40001) surfaces in that row's error receipt; retries stay
the caller's, per the runtime doctrine.
