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
#[derive(Debug)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use morpholog_core::ir_builder::predicate;

    /// Every scalar kind the contract says is filterable, decoded from
    /// the bare text a command line can carry.
    ///
    /// The Boolean row is why this table exists: `--where settled=true`
    /// failed with "received string; expected `true` or `false`" - an
    /// error naming exactly what the caller had written - because the
    /// shared codec takes JSON booleans and a command line has only
    /// text. Nothing refused Bool, so the contract implied support the
    /// code did not have, and no test asked.
    #[test]
    fn every_supported_kind_resolves_from_bare_text() {
        let decl = predicate("Every")
            .subject("subject_field")
            .decimal("decimal_field")
            .date("date_field")
            .timestamp("timestamp_field")
            .duration("duration_field")
            .boolean("bool_field")
            .build();
        let cases = [
            ("subject_field=acct_1", "subject_field"),
            ("decimal_field=13.50", "decimal_field"),
            ("date_field=2026-06-01", "date_field"),
            ("timestamp_field=2026-06-01T12:00:00Z", "timestamp_field"),
            ("duration_field=PT6H", "duration_field"),
            ("bool_field=true", "bool_field"),
            ("bool_field=false", "bool_field"),
        ];
        for (raw, field) in cases {
            let resolved = resolve(&decl, &[raw.to_string()])
                .unwrap_or_else(|e| panic!("`{raw}` must resolve: {e:#}"));
            assert_eq!(
                resolved.len(),
                1,
                "`{raw}` resolves to one filter on {field}"
            );
        }
    }

    #[test]
    fn only_a_decimal_compares_numerically() {
        let decl = predicate("Two").subject("s").decimal("d").build();
        let subject = resolve(&decl, &["s=x".to_string()]).unwrap();
        let decimal = resolve(&decl, &["d=1.5".to_string()]).unwrap();
        assert!(!subject[0].is_numeric());
        assert!(decimal[0].is_numeric(), "scale must not decide equality");
    }

    #[test]
    fn an_undeclared_field_names_the_declared_ones() {
        let decl = predicate("Line").subject("line").subject("invoice").build();
        let err = resolve(&decl, &["invoice_id=x".to_string()]).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("line, invoice"), "got: {text}");
    }
}
