//! Spike step 6: compile-coverage census over every worked example.
//! Pure (no PG). The census is the coverage number the verdict quotes;
//! the hard assertion is that the target programme (03) is 100%
//! compilable. The rest is recorded, not gated - the fragment is
//! deliberately partial.

use morpholog_postgres::spike::compile_invariants;

#[test]
fn census_over_all_programs() {
    let mut lines = Vec::new();
    let mut target_ok = false;
    for program in morpholog_examples::all_programs() {
        let name = program.name.clone();
        let total = program.invariants.len();
        match compile_invariants(&program) {
            Ok(set) => {
                assert_eq!(set.invariants.len(), total);
                lines.push(format!("{name}: {total}/{total} compiled"));
                if name.as_str() == "double_entry_ledger" {
                    target_ok = true;
                }
            }
            Err(refusals) => {
                let compiled = total - refusals.len();
                lines.push(format!("{name}: {compiled}/{total} compiled"));
                for r in &refusals {
                    lines.push(format!("  refused {}: {}", r.invariant, r.reason));
                }
                assert_ne!(
                    name.as_str(),
                    "double_entry_ledger",
                    "the target programme must be 100% compilable: {refusals:?}"
                );
            }
        }
    }
    println!("=== spike compile census ===");
    for line in &lines {
        println!("{line}");
    }
    assert!(target_ok, "double_entry_ledger missing from the corpus");
}
