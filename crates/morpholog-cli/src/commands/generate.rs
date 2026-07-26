//! `morpholog generate python-client` - emit a typed, stdlib-only
//! Python client for a `.morph` programme.
//!
//! The package is five files under `<out>/morpholog_client/`. Three
//! are programme-independent and emitted VERBATIM from the templates
//! beside this crate (`templates/python_client/`), so the files the
//! template test suite runs are byte-identical to the files embedders
//! receive. Two are generated per programme: `models.py` (a frozen
//! request dataclass per transformation, a read model per predicate,
//! a payload per intent) and `__init__.py` (the version-floor check
//! and the stamps - model hash, binary version - that let an
//! embedder's CI assert the generated code, the manifest, and the
//! live binary all name the same rules).
//!
//! Models read `ParamKind` and the declarations directly, never the
//! JSON schema fragments: the exhaustive matches below are forced
//! open by the compiler when a kind is added, where re-parsing JSON
//! would silently misclassify it.
//!
//! Refusal is whole-run: every transformation parameter, predicate
//! field, and intent field is swept BEFORE anything is written, and
//! any unsupported kind or un-emittable name fails the run with every
//! finding listed and the out directory untouched. No partial
//! packages, no silent mangling.

use std::fmt::Write as _;

use anyhow::Context;
use morpholog_core::{
    ArgDecl, ParamKind, PredicateArgKind, Program, ValidatedProgram, Var,
    transformation_param_kinds,
};

use crate::GeneratePythonClientArgs;
use crate::commands::{parse_or_exit, validate_or_exit};

const VALUES_PY: &str = include_str!("../../templates/python_client/values.py");
const ENVELOPES_PY: &str = include_str!("../../templates/python_client/envelopes.py");
const ADAPTER_PY: &str = include_str!("../../templates/python_client/adapter.py");

/// The interpreter floor the emitted package declares and enforces at
/// import. A conservative-subset floor, moved only deliberately - the
/// PG 17+ idiom applied to Python.
const PYTHON_FLOOR: (u32, u32) = (3, 10);

pub(crate) fn run(args: &GeneratePythonClientArgs) -> anyhow::Result<()> {
    let parsed = parse_or_exit(&args.file)?;
    let validated = validate_or_exit(&parsed);
    let program = &parsed.program;

    let refusals = sweep(program, &validated)?;
    if !refusals.is_empty() {
        for refusal in &refusals {
            eprintln!("error: {refusal}");
        }
        eprintln!(
            "generate python-client refused: {} finding(s); nothing was written",
            refusals.len()
        );
        std::process::exit(1);
    }

    // Render everything in memory before touching the filesystem, so
    // no SEMANTIC failure can leave a partial package behind (an IO
    // failure mid-write can, like any file copy; regenerating is the
    // recovery either way).
    let models = render_models(program, &validated)?;
    let init = render_init(program);
    let files = [
        ("__init__.py", init.as_str()),
        ("models.py", models.as_str()),
        ("values.py", VALUES_PY),
        ("envelopes.py", ENVELOPES_PY),
        ("adapter.py", ADAPTER_PY),
    ];

    let package_dir = args.out.join("morpholog_client");
    if args.check {
        return report_drift(&package_dir, &files);
    }

    std::fs::create_dir_all(&package_dir)?;
    for (name, content) in files {
        std::fs::write(package_dir.join(name), content)?;
    }
    eprintln!(
        "generated {} ({} transformations, {} predicates, {} intents)",
        package_dir.display(),
        program.transformations.len(),
        program.predicates.len(),
        program.intents.len(),
    );
    Ok(())
}

/// `--check`: compare the rendered package against what is on disk and
/// write nothing.
///
/// The contract is the EXIT CODE - zero when every file agrees,
/// non-zero on any difference, missing file, or unreadable directory.
/// The prose on stderr names what drifted for a human reading a failed
/// CI log; it is deliberately not a machine surface, so there is
/// nothing here for an embedder to parse and nothing to drift silently
/// (see "A consumed surface is a pinned envelope" in
/// `docs/embedder-integration.md` - this command answers with its
/// status, not with data).
fn report_drift(package_dir: &std::path::Path, files: &[(&str, &str)]) -> anyhow::Result<()> {
    let mut drifted: Vec<String> = Vec::new();
    for (name, expected) in files {
        let path = package_dir.join(name);
        match std::fs::read_to_string(&path) {
            Ok(found) if found == *expected => {}
            Ok(_) => drifted.push(format!("{name}: differs")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                drifted.push(format!("{name}: missing"));
            }
            Err(e) => drifted.push(format!("{name}: unreadable ({e})")),
        }
    }
    if drifted.is_empty() {
        eprintln!(
            "{} is current ({} files)",
            package_dir.display(),
            files.len()
        );
        return Ok(());
    }
    for entry in &drifted {
        eprintln!("error: {entry}");
    }
    eprintln!(
        "{} is stale: {} of {} file(s) drifted; regenerate without --check",
        package_dir.display(),
        drifted.len(),
        files.len(),
    );
    std::process::exit(1);
}

// ============================================================
// The refusal sweep.
// ============================================================

/// Python's hard keywords (3.10 floor). A field with one of these
/// names cannot be a dataclass field; refusing beats mangling, which
/// would silently divorce the field name from the wire name.
const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// Member names the generated classes define; a field sharing one
/// would shadow it. The uppercase entries are the ClassVar metadata
/// slots - `TRANSFORMATION` is an unlikely field name but a lawful
/// `.morph` identifier, and a collision there corrupts the very
/// metadata `submit()` dispatches on.
const RESERVED_MEMBERS: &[&str] = &[
    "to_args_named",
    "from_named",
    "from_args",
    "TRANSFORMATION",
    "PREDICATE",
    "INTENT",
    "_ARG_ORDER",
];

fn kind_supported(kind: &PredicateArgKind) -> bool {
    matches!(
        kind,
        PredicateArgKind::Subject
            | PredicateArgKind::Decimal
            | PredicateArgKind::Date
            | PredicateArgKind::Timestamp
            | PredicateArgKind::Bool
            | PredicateArgKind::Quantity(_)
    )
}

fn name_refusal(owner: &str, name: &str) -> Option<String> {
    let emittable = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !emittable {
        return Some(format!(
            "{owner}: `{name}` is not an emittable Python identifier"
        ));
    }
    if PYTHON_KEYWORDS.contains(&name) {
        return Some(format!(
            "{owner}: `{name}` is a Python keyword; rename the field"
        ));
    }
    if RESERVED_MEMBERS.contains(&name) {
        return Some(format!(
            "{owner}: `{name}` collides with a generated member name; rename the field"
        ));
    }
    None
}

/// Param kinds for a transformation the programme itself declares -
/// the lookup cannot miss, but the error path stays a clean failure
/// rather than a panic in the binary.
fn param_kinds(
    validated: &ValidatedProgram<'_>,
    name: &morpholog_core::TransformationName,
) -> anyhow::Result<Vec<(Var, ParamKind)>> {
    transformation_param_kinds(validated, name)
        .with_context(|| format!("inferring parameter kinds for `{name}`"))
}

/// Collect every reason this programme cannot be generated for, across
/// the whole surface, so the author sees one complete work list.
fn sweep(program: &Program, validated: &ValidatedProgram<'_>) -> anyhow::Result<Vec<String>> {
    let mut refusals = Vec::new();
    for transformation in &program.transformations {
        let owner = format!("transformation `{}`", transformation.name);
        let kinds = param_kinds(validated, &transformation.name)?;
        for (param, kind) in &kinds {
            if let Some(refusal) = name_refusal(&owner, param.as_str()) {
                refusals.push(refusal);
            }
            match kind {
                ParamKind::Concrete(concrete) if kind_supported(concrete) => {}
                ParamKind::Concrete(concrete) => refusals.push(format!(
                    "{owner}: parameter `{param}` has kind {concrete}, which the generated \
                     client does not carry yet (it arrives when a worked embedder forces it)"
                )),
                ParamKind::Polymorphic | ParamKind::Unconstrained | ParamKind::Ambiguous(_) => {
                    refusals.push(format!(
                        "{owner}: parameter `{param}` has no single concrete kind; a typed \
                         request model cannot choose a branch safely (the same rule as \
                         --args-named)"
                    ));
                }
                // A collection with a supported scalar element is carried as
                // a typed list; an element kind the client cannot type (or a
                // nested collection) is refused, the same floor as a scalar.
                ParamKind::Collection(element) => match element.as_ref() {
                    ParamKind::Concrete(c) if kind_supported(c) => {}
                    _ => refusals.push(format!(
                        "{owner}: parameter `{param}` is a collection whose item kind the \
                         generated client cannot type (it must be a single supported scalar)"
                    )),
                },
            }
        }
    }
    for predicate in &program.predicates {
        sweep_decl(
            &format!("predicate `{}`", predicate.name),
            &predicate.args,
            &mut refusals,
        );
    }
    for intent in &program.intents {
        sweep_decl(
            &format!("intent `{}`", intent.name),
            &intent.args,
            &mut refusals,
        );
    }

    // camel() is many-to-one (`capture_trade` and `CaptureTrade` both
    // render `CaptureTradeRequest`), and Morpholog's duplicate check
    // is on exact names - so two lawful declarations can collide at
    // the generated class. Refuse with both sources named; the suffix
    // keeps the three categories disjoint from one another.
    sweep_class_collisions(
        "transformation",
        "Request",
        program.transformations.iter().map(|t| t.name.as_str()),
        &mut refusals,
    );
    sweep_class_collisions(
        "predicate",
        "Claim",
        program.predicates.iter().map(|p| p.name.as_str()),
        &mut refusals,
    );
    sweep_class_collisions(
        "intent",
        "Payload",
        program.intents.iter().map(|i| i.name.as_str()),
        &mut refusals,
    );
    Ok(refusals)
}

fn sweep_class_collisions<'a>(
    category: &str,
    suffix: &str,
    names: impl Iterator<Item = &'a str>,
    refusals: &mut Vec<String>,
) {
    let mut by_class: std::collections::BTreeMap<String, Vec<&str>> =
        std::collections::BTreeMap::new();
    for name in names {
        by_class
            .entry(format!("{}{suffix}", camel(name)))
            .or_default()
            .push(name);
    }
    for (class, sources) in by_class {
        if sources.len() > 1 {
            refusals.push(format!(
                "{category}s {} all generate class `{class}`; rename one",
                sources
                    .iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ));
        }
    }
}

fn sweep_decl(owner: &str, args: &[ArgDecl], refusals: &mut Vec<String>) {
    for arg in args {
        if let Some(refusal) = name_refusal(owner, &arg.name) {
            refusals.push(refusal);
        }
        if !kind_supported(&arg.kind) {
            refusals.push(format!(
                "{owner}: field `{}` has kind {}, which the generated client does not \
                 carry yet (it arrives when a worked embedder forces it)",
                arg.name, arg.kind
            ));
        }
    }
}

// ============================================================
// Rendering.
// ============================================================

/// `capture_trade` -> `CaptureTrade`; already-camel names pass through.
fn camel(name: &str) -> String {
    name.split('_')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// The Python annotation, the named-read parse expression, and the
/// docstring qualifier for one supported kind. The exhaustive match is
/// the point: a new kind fails compilation here instead of emitting a
/// wrong model.
fn kind_map(kind: &PredicateArgKind) -> (&'static str, String, Option<String>) {
    match kind {
        PredicateArgKind::Subject => ("str", "raw".to_string(), None),
        PredicateArgKind::Decimal => ("Decimal", "values.parse_decimal(raw)".to_string(), None),
        PredicateArgKind::Date => ("date", "values.parse_date(raw)".to_string(), None),
        PredicateArgKind::Timestamp => {
            ("datetime", "values.parse_timestamp(raw)".to_string(), None)
        }
        PredicateArgKind::Bool => ("bool", "raw".to_string(), None),
        PredicateArgKind::Quantity(unit) => (
            "Decimal",
            "values.parse_decimal(raw)".to_string(),
            Some(format!(
                "amount in {unit} (the declaration carries the unit)"
            )),
        ),
        PredicateArgKind::Collection | PredicateArgKind::Duration | PredicateArgKind::Any => {
            unreachable!("the refusal sweep rejected unsupported kinds before rendering")
        }
    }
}

fn module_header(out: &mut String, what: &str, program: &Program) {
    let _ = writeln!(
        out,
        "\"\"\"{what} for programme `{}`.\n\nGENERATED by `morpholog generate python-client` - do not edit;\nregenerate when the programme changes. The model hash in\n`__init__.py` names the rules this client was built against.\n\"\"\"\n",
        program.name
    );
}

fn render_models(program: &Program, validated: &ValidatedProgram<'_>) -> anyhow::Result<String> {
    let mut out = String::new();
    module_header(&mut out, "Typed request, read, and payload models", program);
    out.push_str(
        "from __future__ import annotations\n\n\
         from dataclasses import dataclass\n\
         from datetime import date, datetime\n\
         from decimal import Decimal\n\
         from typing import ClassVar\n\n\
         from . import values\n",
    );

    // Request models: one frozen dataclass per transformation, fields
    // in declaration order (the same order x-morpholog-arg-order
    // carries), each knowing its transformation name and how to encode
    // itself for --args-named.
    for transformation in &program.transformations {
        let kinds = param_kinds(validated, &transformation.name)?;
        let class = format!("{}Request", camel(transformation.name.as_str()));
        let _ = write!(
            out,
            "\n\n@dataclass(frozen=True)\nclass {class}:\n    \"\"\"Arguments for `{}`.\"\"\"\n\n    TRANSFORMATION: ClassVar[str] = \"{}\"\n\n",
            transformation.name, transformation.name
        );
        let mut encodes = Vec::new();
        for (param, kind) in &kinds {
            // The sweep already refused everything but a concrete scalar
            // or a collection of a concrete scalar; a collection becomes a
            // typed `list[...]` field encoded item by item.
            let (annotation, qualifier, encode) = match kind {
                ParamKind::Concrete(concrete) => {
                    let (annotation, _, qualifier) = kind_map(concrete);
                    (
                        annotation.to_string(),
                        qualifier,
                        format!("            \"{param}\": values.encode_named(self.{param}),"),
                    )
                }
                ParamKind::Collection(element) => {
                    let ParamKind::Concrete(concrete) = element.as_ref() else {
                        unreachable!("the sweep rejected collections of non-concrete elements")
                    };
                    let (item_annotation, _, qualifier) = kind_map(concrete);
                    (
                        format!("list[{item_annotation}]"),
                        qualifier,
                        format!(
                            "            \"{param}\": [values.encode_named(x) for x in self.{param}],"
                        ),
                    )
                }
                _ => unreachable!("the refusal sweep rejected non-concrete parameter kinds"),
            };
            let _ = writeln!(out, "    {param}: {annotation}");
            if let Some(qualifier) = qualifier {
                let _ = writeln!(out, "    # {qualifier}");
            }
            encodes.push(encode);
        }
        let _ = write!(
            out,
            "\n    def to_args_named(self) -> dict[str, object]:\n        return {{\n{}\n        }}\n",
            encodes.join("\n")
        );
    }

    // Read models: one per predicate, parsing the named read's
    // wire-true values by declared kind.
    for predicate in &program.predicates {
        let class = format!("{}Claim", camel(predicate.name.as_str()));
        let _ = write!(
            out,
            "\n\n@dataclass(frozen=True)\nclass {class}:\n    \"\"\"One admitted `{}` claim, decoded by declared kind.\"\"\"\n\n    PREDICATE: ClassVar[str] = \"{}\"\n\n",
            predicate.name, predicate.name
        );
        for arg in &predicate.args {
            let (annotation, _, qualifier) = kind_map(&arg.kind);
            let _ = writeln!(out, "    {}: {annotation}", arg.name);
            if let Some(qualifier) = qualifier {
                let _ = writeln!(out, "    # {qualifier}");
            }
        }
        let mut parses = Vec::new();
        for arg in &predicate.args {
            let (_, parse, _) = kind_map(&arg.kind);
            parses.push(format!(
                "        raw = args[\"{name}\"]\n        {name} = {parse}",
                name = arg.name,
                parse = parse,
            ));
        }
        let field_list = predicate
            .args
            .iter()
            .map(|a| format!("{name}={name}", name = a.name))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(
            out,
            "\n    @classmethod\n    def from_named(cls, args: dict[str, object]) -> {class}:\n{}\n        return cls({field_list})\n",
            parses.join("\n")
        );
    }

    // Intent payloads: positional args become named typed fields, the
    // contract baked at generation time under the hash stamp - no
    // runtime `schema --intent` call, no hand-coded order. `from_args`
    // takes the DECODED positional values the adapter's OutboxRow
    // already carries (decode_tagged is the adapter's job, arity and
    // naming are the contract's).
    for intent in &program.intents {
        let class = format!("{}Payload", camel(intent.name.as_str()));
        let order = intent
            .args
            .iter()
            .map(|a| format!("\"{}\"", a.name))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(
            out,
            "\n\n@dataclass(frozen=True)\nclass {class}:\n    \"\"\"Payload of an emitted `{}` intent.\"\"\"\n\n    INTENT: ClassVar[str] = \"{}\"\n    _ARG_ORDER: ClassVar[tuple] = ({order}{trailing})\n\n",
            intent.name,
            intent.name,
            trailing = if intent.args.is_empty() { "" } else { "," },
        );
        for arg in &intent.args {
            let (annotation, _, qualifier) = kind_map(&arg.kind);
            let _ = writeln!(out, "    {}: {annotation}", arg.name);
            if let Some(qualifier) = qualifier {
                let _ = writeln!(out, "    # {qualifier}");
            }
        }
        let _ = write!(
            out,
            "\n    @classmethod\n    def from_args(cls, args: list[object]) -> {class}:\n        \"\"\"Build from the decoded positional values of an outbox\n        row's `arguments` (the adapter decodes; this names).\"\"\"\n        if len(args) != {arity}:\n            raise ValueError(\n                f\"{intent}: payload arity {{len(args)}} != contract arity {arity} \"\n                f\"(schema/payload skew)\"\n            )\n        return cls(*args)\n",
            arity = intent.args.len(),
            intent = intent.name,
        );
    }

    // One dispatch table: the deliverer looks payloads up by the
    // intent name the outbox row carries. Requests and reads are
    // reached by class name (the caller knows which model it wants);
    // tables for them arrive when a consumer does.
    let payload_entries = program
        .intents
        .iter()
        .map(|i| format!("    \"{}\": {}Payload,", i.name, camel(i.name.as_str())))
        .collect::<Vec<_>>()
        .join("\n");
    let _ = write!(out, "\n\nINTENT_PAYLOADS = {{\n{payload_entries}\n}}\n");
    Ok(out)
}

fn render_init(program: &Program) -> String {
    let hash = crate::commands::hash::canonical_hash(program);
    let version = env!("CARGO_PKG_VERSION");
    let (floor_major, floor_minor) = PYTHON_FLOOR;
    let mut out = String::new();
    module_header(&mut out, "A typed Morpholog client", program);
    let _ = write!(
        out,
        "import sys\n\n\
         if sys.version_info < ({floor_major}, {floor_minor}):\n    \
             raise RuntimeError(\n        \
                 f\"morpholog_client requires Python {floor_major}.{floor_minor}+ \"\n        \
                 f\"(running {{sys.version_info.major}}.{{sys.version_info.minor}}); \"\n        \
                 f\"the generated code holds a conservative floor on purpose\"\n    )\n\n\
         PROGRAM = \"{program_name}\"\n\
         MODEL_HASH = \"{hash}\"\n\
         MORPHOLOG_VERSION = \"{version}\"\n\
         PYTHON_FLOOR = ({floor_major}, {floor_minor})\n\n\
         from . import envelopes, models, values  # noqa: E402\n\
         from .adapter import Morpholog, MorphologError  # noqa: E402\n\n\
         __all__ = [\n    \
             \"PROGRAM\",\n    \
             \"MODEL_HASH\",\n    \
             \"MORPHOLOG_VERSION\",\n    \
             \"PYTHON_FLOOR\",\n    \
             \"Morpholog\",\n    \
             \"MorphologError\",\n    \
             \"envelopes\",\n    \
             \"models\",\n    \
             \"values\",\n]\n",
        program_name = program.name,
    );
    out
}
