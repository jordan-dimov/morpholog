//! `--where field=value`: argument-level selection for the read surfaces.
//!
//! Field names resolve through the programme's `predicate` declaration,
//! which derived and admitted claims both have. An undeclared field is an
//! error naming the fields that exist, never an empty result that looks
//! like "no matching rows".
//!
//! Equality only; repeated filters must all hold. No read has needed
//! ranges or `or` yet.

use anyhow::{Context, anyhow, bail};
use morpholog_core::{EvalValue, PredicateArgKind, PredicateDecl, Program};

/// One resolved filter: which argument position to compare, and the
/// value to compare it against, already decoded to the declared kind.
#[derive(Debug)]
pub(crate) struct FieldFilter {
    pub(crate) position: usize,
    pub(crate) value: EvalValue,
}

impl FieldFilter {
    /// Whether this value must compare as a number rather than as stored
    /// JSON. Decimals are stored as exact strings, so `13.5` and `13.50`
    /// differ as text but are the same number.
    pub(crate) fn is_numeric(&self) -> bool {
        matches!(self.value, EvalValue::Decimal(_))
    }
}

/// Split `field=value` pairs and resolve each against `decl`.
///
/// Values decode through the `--args-named` codec, so
/// `--where volume_kwh=431.7` is the declared decimal, not the string
/// "431.7" (which would match nothing).
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
            // A quantity compares as a number in memory but as text in the
            // database, so the same filter could answer two ways. Refused.
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

/// Resolve a `where` clause for any surface that offers one. Needs a
/// programme and exactly one declared predicate; refuses before any
/// database work, so the error is about the request, not an empty result.
/// Returns the filters and the arity from the same declaration. No pairs,
/// no filters.
pub(crate) fn resolve_where(
    program: Option<&Program>,
    predicates: &[String],
    pairs: &[String],
) -> anyhow::Result<(Vec<FieldFilter>, i32)> {
    if pairs.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let Some(program) = program else {
        bail!(
            "`where` needs the named read: a field name is resolved against a declaration, \
             and without one there is nothing to resolve it against"
        );
    };
    let [predicate] = predicates else {
        bail!(
            "`where` needs exactly one predicate, because the field names belong to one \
             claim shape; got {}",
            predicates.len()
        );
    };
    let decl = program
        .predicate(predicate)
        .ok_or_else(|| anyhow!("predicate `{predicate}` is not declared in the programme"))?;
    let declared_arity = i32::try_from(decl.args.len())
        .with_context(|| format!("`{predicate}` declares too many arguments to filter"))?;
    let filters = resolve(decl, pairs)?;
    Ok((filters, declared_arity))
}

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
    /// the bare text a command line can carry. Booleans matter most: the
    /// shared codec wants JSON booleans, and a command line has only text.
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
