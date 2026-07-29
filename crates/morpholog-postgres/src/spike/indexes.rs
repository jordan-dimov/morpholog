//! SPIKE: derive partial expression indexes from a compiled set - one
//! per (predicate, argument position) used as a join or case key, so
//! every compiled residual has something to seek on. Text extractor
//! only (the corpus keys are subjects); never ambient - callers apply
//! or drop explicitly per measurement cell.

use std::collections::BTreeSet;

use super::compile::CompiledInvariantSet;
use crate::sql_quote::{quote_ident, quote_literal};

fn key_set(set: &CompiledInvariantSet) -> BTreeSet<(String, usize)> {
    let mut keys = BTreeSet::new();
    for inv in &set.invariants {
        for (predicate, position) in inv.key_positions() {
            keys.insert((predicate.as_str().to_string(), position));
        }
    }
    keys
}

fn index_name(predicate: &str, position: usize) -> String {
    format!("morpholog_spike_{}_a{position}", predicate.to_lowercase())
}

/// `CREATE INDEX` statements for every key position, idempotent.
pub fn spike_index_sql(set: &CompiledInvariantSet) -> Vec<String> {
    key_set(set)
        .into_iter()
        .map(|(predicate, position)| {
            format!(
                "CREATE INDEX IF NOT EXISTS {} ON morpholog.claims ((arguments -> {position} ->> 'value')) WHERE predicate_name = {}",
                quote_ident(&index_name(&predicate, position)),
                quote_literal(&predicate),
            )
        })
        .collect()
}

/// `DROP INDEX` statements for the same set, idempotent - so index
/// state is asserted per run, never inherited from a prior one.
pub fn drop_spike_index_sql(set: &CompiledInvariantSet) -> Vec<String> {
    key_set(set)
        .into_iter()
        .map(|(predicate, position)| {
            format!(
                "DROP INDEX IF EXISTS morpholog.{}",
                quote_ident(&index_name(&predicate, position)),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spike::compile_invariants;

    #[test]
    fn ledger_index_set_is_pinned() {
        let set =
            compile_invariants(&morpholog_examples::double_entry_ledger::program()).unwrap();
        let sql = spike_index_sql(&set);
        assert_eq!(
            sql,
            [
                "CREATE INDEX IF NOT EXISTS \"morpholog_spike_journalentry_a0\" ON morpholog.claims ((arguments -> 0 ->> 'value')) WHERE predicate_name = 'JournalEntry'",
                "CREATE INDEX IF NOT EXISTS \"morpholog_spike_journalline_a0\" ON morpholog.claims ((arguments -> 0 ->> 'value')) WHERE predicate_name = 'JournalLine'",
                "CREATE INDEX IF NOT EXISTS \"morpholog_spike_supersedes_a0\" ON morpholog.claims ((arguments -> 0 ->> 'value')) WHERE predicate_name = 'Supersedes'",
                "CREATE INDEX IF NOT EXISTS \"morpholog_spike_supersedes_a1\" ON morpholog.claims ((arguments -> 1 ->> 'value')) WHERE predicate_name = 'Supersedes'",
            ]
        );
    }
}
