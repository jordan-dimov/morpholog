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

The embedder asks `morpholog schema <file> <transformation>` for the JSON Schema of the transformation's argument object. For the transformations whose parameters all resolve to unambiguous scalar kinds (`Subject`, `Decimal`, `Decimal[U]`, `Date`, `Timestamp`, `Duration`, `Bool`) or collections of them, it sends that shape to `morpholog propose --args-named` to commit (or to `morpholog explain --args-named --json` to dry-run and diagnose). For the corner cases the named codec cannot decode unambiguously - `Polymorphic`, `Unconstrained`, `Ambiguous`, and opaque `Collection` parameters - it falls back to `--args` with the tagged `EvalValue` codec. The named codec's refusals are documented below alongside each pointer to `--args`. Between transitions, `morpholog inspect claims --predicate` reads governed state back - decoded by declared field name under `--named` - so the next transition can be built from claims the embedder did not itself mint. On day zero, `morpholog init` provisions the schema from the binary itself. The embedder parses the JSON envelope on stdout. None of that requires Rust. The `.morph` source is the single source of truth; everything else is derivable from it.

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
- **`Collection`** - a JSON array, decoded item by item against the element kind inferred from how the parameter is iterated (`for` / `forall`): a collection of subjects accepts `["acct_a", "acct_b"]`, a collection of amounts accepts an array of decimal strings, and so on. A collection whose element kind the model never observes (iterated with a binding used at no kind-bearing position, or passed only to a `Collection`-declared predicate argument) stays opaque and is refused here - send it via `--args` with the tagged `EvalValue` codec.
- **`Polymorphic`** / **`Unconstrained`** / **`Ambiguous`** - refused. The schema does not give a single unambiguous kind, so the named codec cannot choose one safely. Use `--args` with the tagged `EvalValue` codec for these parameters.

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

The full codec, faithful and unambiguous. Use this when the schema cannot narrow a parameter's kind (the `--args-named` refusals above), when sending an opaque collection (one whose element kind the model does not observe), or when the embedder genuinely wants the lower-level codec.

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

A rejected envelope carries `rule`: the refused rule's stable identifier - an invariant's name, or a named gate's. Hold that, not `reason`. The reason string is prose for a human and includes rendered expression text, so anything asserting on it breaks the moment a rule is reworded; `rule` is the author's own name and does not move. The key is **absent** for a gate with no name, never filled with the expression, so a value read from `rule` is always safe to compare.

On a single-proposal rejection whose reason names an invariant declared in the source, stderr carries a courtesy line locating the rule (`rule at <file>:<line>:<col> (<name>)`). It is for the human at the terminal, not the integration: parse stdout only. Batch mode never prints it - receipts are the whole contract there.

### `morpholog inspect rejections`

Refused proposals, oldest first, as `rejection_row` objects: the transformation and its arguments, the actor, the refusing rule (`kind` + `rule` + `invariant_version`), the reason string, and `witness` - the values the refused rule was reading.

`witness` is **absent** rather than empty when the kernel could not pin the failure to one iteration, and for rows written before the column existed; absence means "not captured", never "captured nothing". `invariant_version` is absent for the gate kinds.

**This is an operational floor, not a ledger.** Writes happen after the rollback, on the pool, at-most-once - a storm or a failed insert can leave a refusal unrecorded, and nothing repairs it. Audit remains the only legitimacy-grade record. Read a row as a lead to follow, never as proof of what did or did not happen; a persisted witness inherits exactly the same standing as the row carrying it.

Sizing, measured on a five-argument claim: a witness is roughly 700 bytes and about two thirds of a row's variable payload, so a hundred thousand refusals carry on the order of 70 MB of witness data. Default-on, with no flag - the whole value is being able to review a refusal after the process that saw it has gone.

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

Selection stops at predicate granularity. Picking one subject's claims out of the result is the embedder's own filtering, and a predicate read returns zero or more claims - multiplicity is the caller's to handle, except where the programme's own invariants pin it (a singleton in-force pointer, say), which is exactly what licenses a simple lookup. Argument-level selection is `--where`, pinned above.

### `morpholog inspect audit` - the projector's tail

The blessed read for downstream projectors (forced by the first real one: an ETRM read side folding transitions into blotters and positions). Stdout is NDJSON - one committed transition per line, in `(committed_at, transition_id)` order, each line an `audit_row` exactly as pinned in the `$defs` (transition id, transformation name, tagged arguments and actor, the invariants checked with their versions, asserted/retracted claims, emitted intents, `committed_at`, and - on every row the runtime writes - the `attestation`). An empty tail is empty stdout, exit 0 - the poll loop's steady state. There is no `--limit` and no `--follow`: the poll loop is the projector's own.

**`--after <transition_id>`** resumes strictly after a previously seen transition - one opaque token, since every commit envelope, batch receipt, and audit line already carries one. An unknown id is a hard error naming it, never a silent restart from zero.

**The resume is lossless, and the guarantee has a mechanism.** `committed_at` is the WRITER's transaction start time (server-evaluated `now()`), while row visibility follows commit order - so a writer still in flight when a reader snapshots leaves a row that sorts BELOW rows the reader already emitted, and a naive cursor would skip it forever. Each invocation therefore computes a resume horizon first - the minimum transaction start over the database's open transactions - and only then takes its snapshot, emitting nothing at or above the horizon. Rows beyond it are withheld, never lost: the next invocation's fresh horizon surfaces them. Preconditions: the reader's role must see the writers' sessions in `pg_stat_activity` (same role, `pg_read_all_stats`, or superuser - insufficient visibility is detected and is an error naming the remedy, never an unsound frontier), and no prepared transactions write audit (PostgreSQL's default `max_prepared_transactions = 0`). Liveness: the horizon trails the oldest open transaction in the database, so a stuck session stalls the tail without ever losing rows.

**On managed PostgreSQL, assert the writer set.** A managed host's own sessions are permanently hidden and `pg_read_all_stats` is not grantable, so the default refuses even when only one role can write audit. `--writer-role <ROLE>` (repeatable; `writer_roles` on the generated client's `audit()`/`audit_named()`/`checkpoint()` - `checkpoint` shares the horizon) is the explicit opt-in: the operator asserts the session roles that write audit, and the horizon is computed over those roles' sessions only. Verified, not trusted, in the same statement as the horizon: every non-superuser role that can hold a session and can write `morpholog.audit` - directly, by inherited membership, or via `SET ROLE` - must be in the assertion; an unknown asserted name refuses as a typo; a hidden session of an asserted role still refuses (the writers' own sessions must be visible, which holds when the asserted role is the connecting role). What stays outside the proof, by the operator's explicit acceptance: superuser writes, and role-configuration changes in the window between the horizon statement and the read snapshot. Without the flag, nothing changes: lossless-or-loud.

**`--named <file.morph>`** decodes each line's asserted/retracted claims into the `named_claim` shape under the programme's authority, with the same hard-error skew contract as the claims read. `arguments` and `emitted_intents` deliberately stay tagged: they belong to the transformation and intent vocabularies (parameter kinds), not predicate declarations, and the asymmetry is stated rather than papered over with a second decoder.

`--as-of` does not apply: the audit table IS the chronological record, and the tail's coordinate is `--after`.

**`attestation`** records how the actor identity was established. Gateway mode - `{"mode":"gateway","authenticated_by":"<role>"}` - means the PostgreSQL-authenticated login role of the proposing connection asserted the actor; it proves who asserted, never that the named actor authorised anything. The value is resolved by the runtime from the connection itself, never supplied by the caller, and it is covered by the Merkle leaf: rewriting or stripping it after the fact breaks `verify`. Rows written before attestation existed carry none and keep their original leaf encoding, so existing checkpoints and packs stay valid. Operational notes for that boundary: a generated client from before the field existed refuses the first attested row by design (the drift tripwire - regenerate the client), and an offline verifier binary from before the field existed recomputes the wrong leaves for attested rows and reports a root mismatch on genuine history - upgrade the verifier before believing it.

### Restricting who may assert an actor

Gateway attestation records which login role vouched for an actor. It does not restrict which actors a role may name, and by default any role may name any actor - which is fine until a rule depends on two actors being different people, at which point one application holding one connection can satisfy it alone.

Two reserved claims change that for a chosen actor. Both are ordinary claims you declare and govern through your own transformations, under your own authority gates; the runtime only recognises their name and shape, exactly as it does `AuditSigningKey`:

```morph
predicate ActorAssertionRestricted(actor: Subject)
predicate ActorAssertionAuthority(actor: Subject, login_role: Subject)
```

`ActorAssertionRestricted(a)` **arms** the actor when the claim is ADMITTED - not when the predicate is declared. An actor with no such claim behaves exactly as before, so adopting this costs no migration: arm the actors that matter, when they matter, and leave the rest alone. Once armed, a proposal naming `a` needs a matching `ActorAssertionAuthority(a, <session_user>)`, and without one the adapter refuses it before evaluating anything - no audit row, no rejection-log row, nothing recorded. An unauthorised assertion is not "that actor proposed and was refused", it is someone claiming to be them, and attribution before authorisation would let a caller manufacture a history of apparent attempts.

**Restrict the actors that hand out authority, first.** Whatever actor your enrolment transformations gate on is the trust root: if it is unrestricted, any application can assert it and grant itself every name underneath. Arm it in the same transition that establishes it - arming first and granting second would leave a gap in which nobody can act as it, including to issue the grant. The rule generalises: every actor permitted to create, withdraw or alter grants must itself be restricted, or the restrictions below it are decoration.

Keep the two apart. If the grants did the arming, retracting the last grant would return the actor to unrestricted at exactly the moment you are revoking access. Here it **locks the actor out**; returning it to unrestricted means retracting the arming claim, which is its own governed act.

Refusal surfaces as an operational failure on `propose` (non-zero exit, nothing on stdout), a per-row error receipt in `--batch`, and the `actor_assertion_unauthorised` code in a session - a receipt, so the session stays healthy.

An admitted policy claim whose shape the runtime cannot read stops every durable proposal with an error, rather than being ignored. That is deliberate and it is where the fail-closed guarantee actually lives: `check` and the library facades refuse a misshapen DECLARATION early, but compensation reaches the kernel with a decomposed transformation and no programme, so only a check keyed off the claims themselves covers every path.

**The boundary, stated plainly.** This binds callers reaching the record through Morpholog. The runtime's writer role holds `INSERT`/`DELETE` on `morpholog.claims` and `INSERT` on `morpholog.audit`, so anything holding those credentials can write claims and attestation-shaped audit rows directly without passing this check. Two identities are genuinely distinct only when the two applications and their credentials are genuinely separate. This is adapter-enforced assertion policy, not proof of authorship. `session_user` is also immune to `SET ROLE` but not to a superuser's `SET SESSION AUTHORIZATION` - the same accepted residue as a superuser writing audit rows.

**For direct SQL readers** (a projector that prefers the table): the `(committed_at, transition_id)` total order is stable - it is load-bearing in `verify`, coverage replay, and the as-of reconstructions, so it cannot drift; the claim JSONB inside `asserted_claims`/`retracted_claims` is the pinned `claim_instance` shape; the column set may grow additively. And `committed_at` is the transaction START instant, not the commit instant - so a direct tail wanting lossless resume must implement the same compute-horizon-first recipe and preconditions described under `--after` above (min `xact_start` over `pg_stat_activity`, snapshot, then page strictly below the horizon), itself - including, on a managed host, the writer-set assertion: filter to the asserted roles' `usesysid` and verify the census in the same statement, as above.

## Stability and what is not pinned

What this document promises:

- The argument codecs (`--args`, `--args-named`).
- The `morpholog schema` output shape, for both transformation arguments and (`--intent`) intent payloads.
- The `morpholog propose` outcome shape, traced and untraced (the `$defs` key keeps its historical `run_outcome` name).
- The `morpholog explain --json` Explanation shape.
- The `morpholog inspect claims --predicate` claim-object shape (predicate name plus tagged positional args).
- `--where field=value` on `inspect claims` and `inspect derived`: repeatable, conjunctive, equality only. Field names resolve against the programme, so the claims form needs `--named <FILE>` and exactly one `--predicate`, and an undeclared field is a hard error naming the ones that exist - never an empty result that reads like "no such rows". The generated client takes it as `where={"invoice_id": "inv_1"}` on `claims_named` and `derived_named`.

  Three properties worth knowing before relying on it. **This reduces transfer, not scanning.** The comparison runs in the database, so rows that cannot match are never sent, decoded, or held in client memory - but the index covers `(predicate_name, arguments)`, not argument positions, so the database still examines every row of the predicate. Lookup cost follows the predicate's size, not the answer's. **The claims filter runs in the database; the derived one does not** - a derived view is computed from claims, so `inspect derived --where` narrows the answer rather than the work, and `--as-of` filters after replay for the same reason. **Decimals compare as numbers, not as text**: `--where net_gbp=13.5` finds a stored `13.50`, because they are the same number and comparing the stored strings would report no such row for a row that exists. Fields whose values have more than one spelling - quantities and collections - are refused rather than silently mismatched.
- The `morpholog init` provisioning contract (embedded schema, day-zero only, refuse-or-skip on an existing schema, never drop or migrate).
- The `propose --explain-on-reject` envelope (rejections gain `explanation` in the `explain --json` shape, computed against the rejecting snapshot; commits unchanged).
- The `inspect claims --named` decoded-claim shape and its hard-error skew contract.
- The `inspect derived` row shapes: the same claim-object array as the claims read (tagged by default; `--named` decodes by declared field name under the same programme's authority, with the same skew contract), composing with `--as-of`. The whole-stdout arrays are pinned as the `claim_instance_array` / `named_claim_array` `$defs` - the claims read shares them.
- The `refresh derived` typed report (`refresh_derived_report`) on stdout: counts, model hash, generation id, and the source-snapshot pair, which is present or absent together and is a coarse freshness marker, never a lossless audit-resume cursor (for resume, read `inspect audit`). The human summary with timings stays on stderr.
- The `inspect audit` NDJSON tail (the `audit_row` / `audit_row_named` line shapes, the `(committed_at, transition_id)` order, `--after`'s lossless-resume semantics and its preconditions).
- The `morpholog hash` output shape and its rules-identity semantics (canonical-source SHA-256; formatting and comments excluded).
- The `morpholog schema --all` manifest shape (program, hash, predicates, transformations, intents; the keyed objects serialise with sorted keys, and declaration order travels in the explicit `transformation_order` / `intent_order` arrays).
- The `morpholog schema --result` outcome-envelope contract (one `$defs` entry per machine-readable envelope).
- The tamper-evidence envelopes: `verify` (`verify_report`: a replay verdict beside a tree verdict), `checkpoint` (`checkpoint_outcome`), and evidence-pack `export` (`evidence_pack`) / `verify` (`tree_verification`, including the `malformed_pack` verdict).
- Windowed evidence packs: `audit export --from-anchor <prior.json>` (or the weaker `--from-tree-size`) exports the interval between an earlier checkpoint and the covering one as a `window_evidence_pack` (v2), and `audit verify-pack` checks it as a `window_verification` verdict. A window pack is **not a smaller prefix pack**: a prefix pack recomputes its root from every row, while a window pack proves two separate things and needs both - an RFC 6962 **consistency proof** (the later checkpoint is an append-only extension of the earlier one, so the prior period was not rewritten) and a per-row **inclusion proof** (each exported row is the genuine suffix at its position). Consistency alone verifies between two roots regardless of any rows; inclusion alone says nothing about append-only continuity. Verdicts: `inconsistent_extension`, `row_not_included`, `anchor_mismatch`, `signature_invalid`, `signature_required`, `malformed`. **The honest boundary**: a window carries only its `[from, to)` rows, so it checks the to-checkpoint's signatures cryptographically but cannot establish governed signing-key *authority* (that needs the `[0, from)` rows that fold `AuditSigningKey` claims) - and it proves log integrity for the interval, never *business* completeness: a reportable event never proposed to Morpholog is invisible to any Merkle proof, which is a modelling and ingestion obligation upstream. It is the integrity/attribution artifact for a REMIT-relevant audit trail - operator evidence, not the ACER submission format, which the participant generates from the governed records. The `--from-anchor` prior is the operator's **trust object**: a 32-byte tree head held out-of-band - an auditor's saved copy, a counterparty's receipt, a timestamping service - so the verify runs against an anchor the database writer could not have reached. Distributing that head on a cadence is operational discipline the runtime cannot enforce; a self-held anchor on the same box proves nothing.
- Selective evidence packs: `audit export --transition <id>` (repeatable) discloses only the named transitions as a `selective_evidence_pack` (v3), each row carrying an inclusion proof under the covering checkpoint; `audit verify-pack` checks it as a `selective_verification` verdict, offline. Undisclosed rows are absent from the pack entirely - the reveal-nothing property is pinned byte-level in the e2e suite. **The honest boundary**: a selective pack proves each disclosed row genuine at its claimed position (the position is proven by the row's Merkle path, never by pack order) - it does NOT prove the selection is *complete* (all rows relevant to a party or obligation; that needs the deferred subject-indexed commitment or an honestly narrower claim), it checks the covering checkpoint's signatures cryptographically only (authority stays a full-prefix property), and the disclosed leaf indices necessarily reveal positions and count. Finding the relevant transition ids (by counterparty, by obligation) is the discloser's read - `inspect claims` / `inspect audit` - which is where the relevance judgment lives.
- Signed tree heads: `checkpoint --signing-key <pkcs8.pem> --key-id <id>` (key pairs from `keygen`) signs the new tree head, so a checkpoint / pack checkpoint carries an optional `signatures` array (`tree_head_signature`: `key_id`, `purpose`, `public_key`, `signature`). A signature that does not verify over its tree head is the `signature_invalid` verdict.
- Keys as claims: a signing key is authorised by an admitted `AuditSigningKey(key_id, purpose, public_key)` claim - the operator declares the predicate (recognised by that name and its exact `Subject` triple) and admits/retracts it through its own transformation under its own authority gate (key rotation is supersession, revocation is retraction; both land in the audit log and travel in evidence packs). `checkpoint --signing-key` refuses to sign with a key not authorised as of the prefix, so an unauthorised or misshapen-declaration key fails at signing time rather than passing then failing verify. Verification resolves the authorised key **as of the checkpoint's own prefix**, live and offline alike, so revocation is non-retroactive: a key valid when a checkpoint was signed stays valid for that checkpoint. A genuine signature by a key the ledger did not authorise as of that prefix is the `unauthorized_key` verdict; a supplied signed anchor's own signatures are verified the same way. The root of trust stays honest: the first authorisation is trusted the way the schema is - signing makes key authority governed, auditable, and revocable, it does not conjure trust from nothing.
- Compliance mode: `audit verify --require-signatures` / `audit verify-pack --require-signatures` fails an otherwise-intact tree that carries any unsigned checkpoint (`signature_required`). Signing is opt-in by default; this is the verifier's policy, applied over the intrinsic verdict.
- Exit-code semantics for `propose`, `explain`, `audit verify`, and `audit verify-pack` (a divergence, tamper, or malformed pack is a decided verdict on stdout at exit one, not an operational failure).

What is deliberately left open, pending the worked example that forces the shape:

- **Nothing about `--trace` any more.** It used to be listed here: the envelope was pinned and its entries were not, so a consumer reading a step was parsing ad-hoc JSON. The entries are now `$defs` entries, golden-pinned against real runtime output, and typed on the generated client.
- **The remaining `morpholog inspect` output shapes.** The claims, audit, derived and rejection shapes are pinned; `outbox` and `guarantees` vary and earn their own contract entries when an embedder leans on them.

## The outcome-envelope contract (`schema --result`)

`morpholog schema --result` emits one JSON Schema (Draft 2020-12) document whose `$defs` pin every machine-readable envelope this document describes in prose: the tagged value encoding, claim and intent instances, the `propose` outcome union (the `run_outcome` def: committed | rejected, the latter optionally carrying `--explain-on-reject`'s `explanation`), the traced envelope, the `explain --json` Explanation with its gate / invariant / error verdicts, batch receipts, the audit tail's row shapes (tagged and `--named`), outbox rows and the claim / complete / release wrappers, the `check --json` report, the `inspect coverage --json` report, the `hash` / `init` reports, the `refresh derived` report, and the tamper-evidence family (the `verify_report`, the `tree_verification` verdicts incl. `signature_invalid` / `unauthorized_key` / `signature_required`, the `checkpoint_outcome` and `checkpoint` with their optional `signatures` array of `tree_head_signature`, the `evidence_pack` with its embedded checkpoints and audit rows, the windowed family - `window_evidence_pack` with its consistency proof and per-row `row_inclusion_proof`s, and the `window_verification` verdicts incl. `inconsistent_extension` / `row_not_included` - and the selective family (`selective_pack_manifest`, `selective_evidence_pack`, `selective_verification`)). Programme-independent - the shapes vary only with the binary, so no `.morph` file is taken and the document is byte-stable for a given build.

This is the artefact client generation consumes (the reserved "waits for a real consumer" slot is now filled): the generated Python client's envelope models derive from the same pinned document, and a contract-test suite in the binary's own repository holds the document, the binary's real serialization, and the generated models to one set of golden envelopes. The `trace` array's items are pinned too (`trace_entry`, a kind-discriminated union over every statement step and the invariant check, with `require_outcome` / `bind_one_outcome` / `for_iteration` beneath it); a `bind`'s bindings use the same `witness_binding` shape a refusal's witness does, because it is the same idea.

## A consumed surface is a pinned envelope (bridges are silent landmines)

The drift check is regenerate-and-diff: regenerate the client (and the SQL views), diff the bytes, and a changed envelope shows up as a changed file. That check is only as wide as the surfaces it covers. A surface an embedder reaches by hand-writing its own subprocess call and parsing the raw JSON - a *bridge* - sits outside it. The envelope can be restructured under a version bump and nothing reddens: not the embedder, not the generated client (its bytes never moved), not Morpholog's own golden set. The break surfaces as a parse failure in production, at the worst possible moment. "The client is byte-stable across the upgrade" then proves only the *generated* surface safe while a bridged one quietly broke.

So the contract is: **every operational surface an embedder consumes is a pinned envelope** - a `$defs` entry in `schema --result`, byte-pinned in the binary's golden set, and emitted as a typed method by `generate python-client`. The three pins move together: an envelope-touching change reddens all three or a test names the drift. A consumed surface that emits ad-hoc JSON has no such floor, so a shape change there is silent by construction. An envelope an embedder depends on therefore carries the same no-silent-change discipline as an invariant: the shape changes deliberately, under review, with a test going red here first - before any embedder feels it.

The read, commit, and tamper-evidence surfaces are all under this contract now: `audit verify` (the `views` verdict included), `audit checkpoint`, and `audit export` / `audit verify-pack` (prefix, window, and selective alike) are pinned in `schema --result`, byte-pinned in the golden set, and emitted as typed methods by `generate python-client` (`audit_verify`, `audit_checkpoint`, `audit_export`, `audit_export_window`, `audit_export_selective`, the offline `audit_verify_pack` / `audit_verify_pack_window` / `audit_verify_pack_selective`) - no bridge between them and the embedder. The `evaluate` score reports (single, batch, and train/test-split forms) are pinned in `schema --result` and the golden set too - they are what a discovery harness consumes - with a generated client method deferred until a harness asks for one. The legitimacy work builds on that contract rather than reopening it.

## The generated Python client (`generate python-client`)

```bash
morpholog generate python-client <file.morph> --out <dir>
```

emits a complete, self-contained `morpholog_client/` package: the value codecs both directions (decimals as `decimal.Decimal` end to end, aware datetimes with naive ones refused on write), the envelope models (key-set strict - an envelope field this client does not know raises instead of dropping data), the subprocess adapter over the whole surface this document pins (including `propose --batch`, `check --json`, `inspect coverage`, the `inspect audit` tail as `audit()` / `audit_named()` with the `--after` resume cursor, `as_of` on both claims reads - a transition id or RFC 3339 timestamp, threading the binary's `--as-of` - the derived reads as `derived()` / `derived_named()` with the same `as_of`, the read-model refresh as `refresh_derived()`, and the tamper-evidence surface as `audit_verify()` / `audit_checkpoint()` / `audit_export()` / `audit_export_window()` / `audit_export_selective()` / the offline `audit_verify_pack()` / `audit_verify_pack_window()` / `audit_verify_pack_selective()`), a frozen request dataclass per transformation with fields in declaration order and `to_args_named()`, a read model per predicate parsing the named read's wire-true values by declared kind, and a payload model per intent with the positional arg order baked in - no runtime `schema --intent` call. `__init__.py` carries the stamps (`PROGRAM`, `MODEL_HASH`, `MORPHOLOG_VERSION`) and enforces the declared Python floor (3.10) at first import.

`--check` compares the would-be output against what is already at `--out` and writes nothing: it exits zero when every file agrees and non-zero otherwise, naming every file that differs or is missing (an absent package is drift, not a pass). **The exit code is the contract**; the stderr lines naming what drifted are for a human reading a failed CI log, deliberately not a machine surface - there is nothing here to parse, so nothing to drift silently. It is the regenerate-into-a-tempdir-and-diff gate both consumer repos wrote by hand, as one subprocess:

```bash
morpholog generate python-client billing.morph --out . --check   # zero, or drift
```

The properties that make it a contract rather than a convenience: **stdlib-only** (no dependency treadmill; richer types are the embedder's to build on top), **deterministic** (the same binary and programme produce byte-identical output, so the drift check is regenerate-and-diff), and **whole-run refusal** (a programme whose contract the client cannot carry - a Duration field, a parameter with no single concrete kind, a Python-keyword field name - fails generation with every finding listed and nothing written; no partial packages, no silent mangling). The worked embedder (`examples/etrm_embedder/`) runs on its committed output.

## Re-provisioning a development database (`init --reset`)

`init` provisions once and never drops or migrates. For a development database that wants a clean slate, `--reset` drops the `morpholog` schema and everything in it, then provisions fresh - the `DROP SCHEMA morpholog CASCADE` that two consumer repos were shelling out to `psql` for.

It is destructive, so the acknowledgement is part of the contract: `--reset` alone refuses (naming the target it would have destroyed, so a mistyped production URL is visible before anything happens), and `--i-know-this-deletes-data` alone refuses too, so a stray acknowledgement left in a script cannot lie in wait for a later `--reset`. The report distinguishes dropping a schema from finding none, rather than implying it removed something that was never there.

The test-fixture shape it is for, once per session:

```python
@pytest.fixture(scope="session")
def governed_db():
    subprocess.run(
        ["morpholog", "init", "--reset", "--i-know-this-deletes-data",
         "--database-url", DEV_URL],
        check=True,
    )
```

## Reading governed state as SQL (`generate views`)

The Python client is the *write* contract; this is the *read* contract for anything that speaks SQL - BI tools, dashboards, the embedder's own projector.

```bash
morpholog generate views <file.morph> [--schema morpholog_views] [--out views.sql]
morpholog generate views x.morph | psql -v ON_ERROR_STOP=1 "$DATABASE_URL"
```

emits one **atomic** SQL script: `BEGIN;`, a `CREATE SCHEMA IF NOT EXISTS`, one `CREATE OR REPLACE VIEW` per declared **base** predicate over `morpholog.claims` and one per **derived** predicate over the `morpholog_read` cache (see "Derived views" below), a `_morpholog_catalog` view recording each view's `kind`, and `COMMIT;`. With `--out` it writes the file; otherwise it prints the script verbatim to stdout (byte-identical, so the pipe-to-`psql` contract holds). Like the Python client, it is **deterministic** (drift check = regenerate-and-diff; a committed golden lives at `examples/10_trade_lifecycle/trade_lifecycle_views.sql`) and **whole-run refusal** (an un-emittable identifier - a non-lowercase or over-63-byte field name, a `_morpholog_`-prefixed field, or two predicates colliding on one snake_case view name - fails with every finding on stderr and nothing written). A SQL reserved word like `limit` is **quoted, not refused** - every generated identifier is double-quoted, so a reserved-word column or view is valid DDL; consumers quote it in their own queries (`SELECT "limit" FROM ...`).

What an embedder needs to know to consume it:

- **Schema and naming.** Views land in `morpholog_views` (override with `--schema`), namespaced away from the governed `morpholog` schema. Predicate names become snake_case view names (`TradeSettled` → `trade_settled`), acronym-aware (`PPAContract` → `ppa_contract`); field names are preserved as declared.
- **Columns.** Provenance first - `_morpholog_asserted_in`, `_morpholog_asserted_at`, and the raw `_morpholog_arguments` (the exact governed JSONB) - then the business fields in declaration order, each cast to its natural PostgreSQL type (Subject→`text`, Decimal→`numeric`, Date→`date`, Timestamp→`timestamptz`, Duration→`interval`, Bool→`boolean`, unit-tagged Quantity→`numeric` over the amount with the unit in a `COMMENT ON COLUMN`). `Collection` and `Any` are exposed as `jsonb` (`Any` as the whole tagged object).
- **Read-only - an interface, not a boundary.** Each view wraps its source in a `WITH`, which makes it non-updatable: `INSERT`/`UPDATE`/`DELETE` through it fail. This stops accidental writes through the read surface; it is *not* a security control - a role with direct write on `morpholog.claims` still writes. Grant accordingly.
- **Precision.** `timestamptz`/`interval` are microsecond-resolution; a Morpholog timestamp/duration can carry nanoseconds. The typed columns truncate below a microsecond; `_morpholog_arguments` always holds the exact source, so read it when sub-microsecond exactness matters.
- **Applying and evolving.** Apply the whole script in one shot - it is transactional, so a failure rolls back rather than leaving a half-built surface. Pipe through `psql -v ON_ERROR_STOP=1` in deployment or CI: without it `psql` continues past a failed statement and exits `0`, so the shell sees success even though the transaction rolled back; `ON_ERROR_STOP=1` makes the shell exit code match the SQL outcome. Across model versions, **appending a predicate field is a compatible `CREATE OR REPLACE`** (the new column appends at the end); **renaming, removing, reordering, or retyping a field is not** - that needs a manual view migration or a `DROP VIEW` first, because `CREATE OR REPLACE VIEW` forbids changing an existing column's name, position, or type.
- **The catalogue and stale views.** `morpholog_views._morpholog_catalog` carries the programme name, the model hash (the *same* `MODEL_HASH` the Python client stamps - pin CI to it), and the predicate→view inventory. The script never drops anything, so a predicate removed between versions leaves its old view behind; the catalogue is the current intended set, and every generated view carries a `COMMENT ON VIEW` ownership marker, so a consumer can find views no longer in the catalogue. (An explicit `--prune` is deferred.)
- **The surface is sealed.** At apply time - inside the same transaction that creates the views - the script records each view's definition hash *as PostgreSQL stores it* (`pg_get_viewdef` read back, the catalogue included) into `_morpholog_view_defs`. `morpholog audit verify --views-schema morpholog_views` cross-checks the catalogue's intended inventory, that seal, and the live views: a view redefined in place under the same name - which the inventory and the model hash cannot see - reports `tampered` naming it, a dropped view or a deleted seal row reports it `missing`, and a surface applied before sealing existed reports `not_sealed` (visible, not a failure). A tampered surface exits one; put the verify in the same CI leg that pins `MODEL_HASH`.
- **Derived views read the cache, base views read claims.** A derived predicate's view is generated alongside the base ones, but it projects the `morpholog_read` cache, not `morpholog.claims`: its provenance columns are `_morpholog_refreshed_at`, `_morpholog_model_hash`, `_morpholog_source_snapshot_transition_id`, `_morpholog_source_snapshot_committed_at`, then `_morpholog_arguments` and the typed business fields. It reads only the active generation **whose model hash matches the generated surface**, so it returns zero rows until `morpholog refresh derived` has run for the *same* `.morph`: no refresh yet, a refresh from a different model, or a stale older-model refresh all read as empty rather than projecting mismatched rows. The catalogue's `kind` column marks each view `base` or `derived`. (The column kinds come straight from the derived claim's declared head predicate - validation already requires it - so no separate inference is involved.)

## The derived read model (`refresh derived`)

Base views project admitted claims. Derived claims are *computed* - so rather than recompute them in SQL (which could not bit-match the kernel's exact decimal/time arithmetic), the kernel computes them and writes the exact result into a read model that SQL can project. SQL is never a second evaluator.

```bash
morpholog refresh derived <file.morph>   # reads --database-url / $DATABASE_URL
```

recomputes every derived claim with the kernel and publishes a new generation into the `morpholog_read` schema - the third namespace alongside governed `morpholog` and the `morpholog_views` BI surface. Stdout is the typed `refresh_derived_report` (pinned above; `refresh_derived()` on the generated client); the human summary with timings stays on stderr. What an embedder needs to know:

- **Out-of-band, by design.** Refresh is never part of `propose` - run it after a batch import or on a schedule (e.g. cron, or after `propose --batch`). Keeping it off the commit path is deliberate: read-model freshness is operational, not part of the governed transition.
- **As of the last refresh.** The projection reflects the claims visible in the refresh's read snapshot; it is stale until the next refresh. The recorded `source_snapshot_*` is the latest audit transition that snapshot saw - a coarse freshness marker, not a lossless audit-resume coordinate (a transaction in flight at snapshot time is excluded and folded in by the next refresh; for lossless resume read `inspect audit`). The kernel's `morpholog inspect derived <file> <Name>` remains the authoritative, always-live read (and the only one that takes `--as-of`). Treat `morpholog_read` as a fast cache, `inspect derived` as ground truth.
- **Exact, not approximate.** Rows store the kernel's exact computed values as tagged JSONB (the same shape as `morpholog.claims`), so a derived `Decimal`, `Quantity`, or `Duration` is the kernel's value, not a SQL re-derivation.
- **Never governed state.** Nothing in `propose`, invariant checking, or `value` lookups reads `morpholog_read`. It is a discardable projection: drop and rebuild it freely.
- **Generation-safe for readers.** A refresh builds a new generation and flips a single-row active pointer; in-flight readers stay on the prior generation until the swap commits, and a failed refresh leaves the previous projection intact. Read the active generation by joining `morpholog_read.derived_claims` to `morpholog_read.derived_active` on `refresh_id` (the generated derived views wrap exactly this).
- **Scope.** Full refresh, exact, single-threaded - sized for operational stores. Partitioned, incremental, and parallel refresh are deferred; the report carries the source/derived counts, and the stderr summary adds per-phase timings so you can see the cost.

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

## The resident session (`morpholog session`)

```bash
morpholog session <file.morph> --database-url ...
```

One process, many operations: parse and validate once, hold one warm
connection, then answer NDJSON requests on stdin with the same pinned
envelopes the one-shot commands print - one compact line per request,
in order. This is the escape from the per-call subprocess and
connection tax when an embedder drives many operations
(`scripts/embedder_latency.sh` measures both paths side by side; a
steady-state session proposal runs a few times faster than a one-shot
`propose` locally, and over a remote link it also removes a
connection handshake per call). The generated Python client wraps it
as a context manager with the same method names the one-shot client
carries, opened through the generated `open_session`:

```python
from morpholog_client import open_session

with open_session("model.morph", DATABASE_URL) as session:
    receipt = session.propose("record_delivery", "meter_gateway", args)
    rows = session.claims_named("DeliveredQty")
```

`open_session` pins the model hash the client package was generated
against, so a binary serving other rules is refused at the handshake
rather than part-way through a run. Constructing `Session` directly
is the lower-level escape hatch for a deliberately unpinned open.

**The protocol is lockstep.** One request line in, one response line
out, strictly in order, no correlation ids. The first line out -
unprompted - is the ready line (`session_ready` in the pinned
schema): `status: "ready"`, the `protocol` number this wire speaks,
and `model_hash`, the canonical rules-identity hash of the programme
the session pinned at startup. The programme is read once: editing
the file does not change a running session, and rolling out a new
model means starting new sessions and draining old ones - the ready
line tells a client what it got; it does not prevent an obsolete
process from continuing to write. A deployment that must not run
against the wrong model asserts the hash at open - which
`open_session` does for you, and `Session(expected_model_hash=...)`
does explicitly.

**Requests.** One JSON object per line, an `op` field naming the
operation, remaining fields exactly the generated client's
parameters. Unknown fields are refused, not ignored - a misspelt
`predciates` must never silently mean "all predicates".

- `{"op": "propose", "transformation": ..., "actor": ...,
  "args"|"args_named": ..., "explain_on_reject"?: true}` - answered
  with the batch receipt shape verbatim (`row` = the 1-based request
  line number).
- `{"op": "claims", "predicates"?: [...], "named"?: true,
  "as_of"?: ..., "where"?: {...}}` - answered with the pinned claim
  array (tagged or named), compact on one line.
- `{"op": "derived", "name": ..., "named"?: true, "as_of"?: ...,
  "where"?: {...}}` - the derived read, same array shapes.

The streaming reads stay one-shot commands: the audit tail holds a
read transaction open and coverage replays the whole log under a
deferrable snapshot - neither fits a lockstep wire.

**Per-request failure is a coded receipt; operational failure aborts.**
A malformed line, an unknown operation or transformation, undecodable
arguments, a serialization conflict, a kernel error, or a colliding
intent answers with `session_error_receipt`: `status: "error"`, the
`row`, the prose, and a stable `code` - because a caller deciding
whether a retry is safe must never parse prose.
`serialization_failure` is the one code that is safe to re-submit on;
the session stays healthy after every coded receipt. An operational
failure (a dead connection, a schema mismatch) aborts the process
with a non-zero exit and no receipt.

**A lost response is an unknown outcome.** Once a propose request has
been written, a session that dies, hangs, or answers garbage leaves
the commit outcome UNKNOWN - the database may have committed before
the failure reached the caller. The generated client poisons the
session (no later call can consume a late line) and raises
`MorphologOutcomeUnknown`, distinct from both a coded refusal
(`MorphologRequestError`) and an ordinary operational error: blind
re-submission after an unknown outcome can duplicate a business
action, so read the record first. Retries stay the caller's in every
case, per the runtime doctrine.

**Attestation is the batch's, documented.** Every commit still
records `authenticated_by` from its own connection's `session_user`
inside the committing transaction - which for a resident process is
the session's role for its whole lifetime, exactly as a batch import
records one role for all its rows. The per-request `actor` remains a
caller assertion. Lineage granularity is therefore the process: a
deployment that needs `authenticated_by` to discriminate callers runs
one session per PostgreSQL role. The session holds exactly one
connection, so many application workers each holding a session stay
bounded on the database.

The whole conversation is pinned: the two new envelopes are in
`schema --result` and the golden set, and a golden transcript
(`tests/golden/session/transcript.ndjson`) pins the request bytes the
generated client emits and the response lines it parses - the Rust
end-to-end test records it against a real database, the Python
session tests replay it.
