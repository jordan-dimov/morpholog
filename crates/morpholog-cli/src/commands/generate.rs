//! `morpholog generate python-client` - emit a typed, stdlib-only
//! Python client for a `.morph` programme.
//!
//! The static modules are copied verbatim from `templates/python_client/`,
//! so the files the template tests run are the files embedders get. Two
//! are generated per programme: `models.py` (a request dataclass per
//! transformation, a read model per predicate, a payload per intent) and
//! `__init__.py` (the version check, plus the model hash and binary
//! version stamps an embedder's CI can compare).
//!
//! Models read `ParamKind` and the declarations directly, not the JSON
//! schema, so a new kind breaks the exhaustive matches below at compile
//! time instead of being misread.
//!
//! Refusal is whole-run: every parameter and field is checked before
//! anything is written, and any unsupported kind or name fails the run
//! with every finding listed and the output directory untouched.

use std::fmt::Write as _;

use anyhow::Context;
use morpholog_core::{
    ArgDecl, ParamKind, PredicateArgKind, Program, ValidatedProgram, Var,
    transformation_param_kinds,
};

use crate::GeneratePythonClientArgs;
use crate::commands::{AlreadyReported, parse_or_report, validate_or_report};

const VALUES_PY: &str = include_str!("../../templates/python_client/values.py");
const ENVELOPES_PY: &str = include_str!("../../templates/python_client/envelopes.py");
const ADAPTER_PY: &str = include_str!("../../templates/python_client/adapter.py");
const SESSION_PY: &str = include_str!("../../templates/python_client/session.py");

/// The oldest Python the emitted package supports, checked at import.
/// Raise it only deliberately.
const PYTHON_FLOOR: (u32, u32) = (3, 10);

pub(crate) fn run(args: &GeneratePythonClientArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    let validated = validate_or_report(&parsed)?;
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
        return Err(AlreadyReported.into());
    }

    // Render everything in memory first, so a refusal never leaves a
    // partial package. (An IO failure mid-write still can; regenerate.)
    let models = render_models(program, &validated)?;
    let init = render_init(program);
    let files = [
        ("__init__.py", init.as_str()),
        ("models.py", models.as_str()),
        ("values.py", VALUES_PY),
        ("envelopes.py", ENVELOPES_PY),
        ("adapter.py", ADAPTER_PY),
        ("session.py", SESSION_PY),
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
/// The exit code is the contract: zero when every file agrees, non-zero on
/// any difference, missing file or unreadable directory. The stderr prose
/// is for a human reading a CI log, not for parsing.
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
    Err(AlreadyReported.into())
}

// ============================================================
// The refusal sweep.
// ============================================================

/// Python's hard keywords (3.10 floor). Such a name cannot be a dataclass
/// field, and renaming it would break the link to the wire name, so it is
/// refused.
const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// Member names the generated classes define; a field sharing one would
/// shadow it. The uppercase entries are class metadata, such as the
/// `TRANSFORMATION` that `submit()` dispatches on.
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

/// Param kinds for a transformation the programme declares. The lookup
/// cannot miss, but fails cleanly rather than panic.
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
                // A collection of a supported scalar becomes a typed list;
                // any other element kind is refused.
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

    // `capture_trade` and `CaptureTrade` both become `CaptureTradeRequest`,
    // so two valid declarations can collide. Refuse, naming both. The
    // suffixes keep transformations, predicates and intents apart.
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

/// The Python annotation, the parse expression for a named read, and the
/// docstring qualifier for one supported kind. The match is exhaustive so
/// a new kind fails to compile here rather than emit a wrong model.
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
        PredicateArgKind::Collection
        | PredicateArgKind::Duration
        | PredicateArgKind::CalendarSpan
        | PredicateArgKind::Any => {
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

    // Request models: one frozen dataclass per transformation, fields in
    // declaration order (as in x-morpholog-arg-order), each able to encode
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
            // Only scalars and collections of scalars get this far; a
            // collection becomes a typed `list[...]` field.
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

    // Read models: one per predicate, parsing values by declared kind.
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

    // Intent payloads: positional args become named typed fields, fixed at
    // generation time, so no runtime `schema --intent` call is needed.
    // `from_args` takes the already-decoded values an OutboxRow carries.
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

    // The deliverer looks payloads up by the intent name on the outbox
    // row. Requests and reads are used by class name, so need no table.
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
         from .adapter import Morpholog, MorphologBatchIncomplete, MorphologError  # noqa: E402\n\
         from .session import (  # noqa: E402\n    \
             MorphologOutcomeUnknown,\n    \
             MorphologRequestError,\n    \
             Session,\n)\n\n\
         def open_session(\n    \
             file: str,\n    \
             database_url: str,\n    \
             *,\n    \
             binary: str | None = None,\n    \
             timeout: float | None = None,\n\
         ) -> Session:\n    \
             \"\"\"Open a session pinned to the programme this package was\n    \
             generated from: a binary serving any other rules is refused at\n    \
             the handshake, before a single proposal is written. Construct\n    \
             ``Session`` directly to open deliberately unpinned.\"\"\"\n    \
             return Session(\n        \
                 file,\n        \
                 database_url,\n        \
                 binary=binary,\n        \
                 timeout=timeout,\n        \
                 expected_model_hash=MODEL_HASH,\n    \
             )\n\n\n\
         __all__ = [\n    \
             \"PROGRAM\",\n    \
             \"MODEL_HASH\",\n    \
             \"MORPHOLOG_VERSION\",\n    \
             \"PYTHON_FLOOR\",\n    \
             \"Morpholog\",\n    \
             \"MorphologBatchIncomplete\",\n    \
             \"MorphologError\",\n    \
             \"MorphologOutcomeUnknown\",\n    \
             \"MorphologRequestError\",\n    \
             \"Session\",\n    \
             \"open_session\",\n    \
             \"envelopes\",\n    \
             \"models\",\n    \
             \"values\",\n]\n",
        program_name = program.name,
    );
    out
}
