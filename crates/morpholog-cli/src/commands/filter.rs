//! `--where field=value`: argument-level selection for the read surfaces.
//!
//! A field name only means something under a declaration, so both users
//! resolve names the same way - through the programme's `predicate`
//! declaration, which a derived claim has as much as an admitted one.
//! That keeps the programme-as-authority contract `--named` already
//! carries: an undeclared field is a hard error naming what does exist,
//! never an empty result that reads like "no matching rows".
//!
//! Equality only, and repeats are conjunctive. Ranges and disjunction are
//! absent because no read has needed them; the shape that arrived was a
//! single field equal to a single value.

use anyhow::{Context, anyhow};
use morpholog_core::{EvalValue, PredicateArgKind, PredicateDecl};

/// One resolved filter: which argument position to compare, and the
/// value to compare it against, already decoded to the declared kind.
pub(crate) struct FieldFilter {
    pub(crate) position: usize,
    pub(crate) value: EvalValue,
}

impl FieldFilter {
    /// Whether this value must compare as a number rather than as stored
    /// JSON. Decimals are stored as strings to stay exact, so `13.5` and
    /// `13.50` are the same number and different text - a filter that
    /// compared the text would report no such row for a row that exists.
    pub(crate) fn is_numeric(&self) -> bool {
        matches!(self.value, EvalValue::Decimal(_))
    }
}

/// Split `field=value` pairs and resolve each against `decl`.
///
/// The value decodes through the same named codec `--args-named` uses, so
/// `--where volume_kwh=431.7` is the declared decimal rather than the
/// string "431.7" - comparing a tagged value to a raw string would match
/// nothing and look like an empty book.
pub(crate) fn resolve(
    decl: &PredicateDecl,
    raw_filters: &[String],
) -> anyhow::Result<Vec<FieldFilter>> {
    raw_filters
        .iter()
        .map(|raw| {
            let (field, value) = raw.split_once('=').ok_or_else(|| {
                anyhow!("`--where {raw}` is not `field=value`; equality is the only comparison")
            })?;
            let position = decl
                .args
                .iter()
                .position(|arg| arg.name == field)
                .ok_or_else(|| {
                    let declared: Vec<&str> = decl.args.iter().map(|a| a.name.as_str()).collect();
                    anyhow!(
                        "`{}` declares no field `{field}`. Declared: {}",
                        decl.name,
                        declared.join(", ")
                    )
                })?;
            let kind = &decl.args[position].kind;
            // A quantity's amount compares as a number in memory and as
            // text in the database, so allowing it would make the same
            // filter answer differently depending on which read served
            // it. Refused until one comparison covers both.
            if matches!(
                kind,
                PredicateArgKind::Quantity(_) | PredicateArgKind::Collection
            ) {
                return Err(anyhow!(
                    "`{}.{field}` is a {} and cannot be filtered yet; \
                     filter on a field whose value has one spelling",
                    decl.name,
                    match kind {
                        PredicateArgKind::Quantity(unit) => format!("quantity in {unit}"),
                        _ => "collection".to_string(),
                    }
                ));
            }
            let value = super::args::decode_declared_value(field, kind, value)
                .with_context(|| format!("`--where {raw}`"))?;
            Ok(FieldFilter { position, value })
        })
        .collect()
}

/// Does this claim's argument list satisfy every filter? Used by the
/// derived reader, which filters after enumeration - a derived view is
/// computed from claims, so the work happens either way.
pub(crate) fn matches(args: &[EvalValue], filters: &[FieldFilter]) -> bool {
    filters
        .iter()
        .all(|f| args.get(f.position).is_some_and(|arg| *arg == f.value))
}
