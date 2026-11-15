//! Which of an act's parameters a refusal's gate reads: directly, or
//! through the `let`, `bind` and `for` statements before it. A static
//! dependency the refusal envelope carries, so a host can show a refusal
//! beside the inputs its rule reads without restating every rule.

#![expect(
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use morpholog_core::related_parameters;

const PROGRAMME: &str = "program related
predicate Part(part: Subject)
predicate PartDescendsFrom(part: Subject, ancestor: Subject)
predicate StockItem(item: Subject, part: Subject, quantity: Decimal)
predicate SalesLine(line: Subject, part: Subject, quantity: Decimal)
predicate Allocation(allocation: Subject, line: Subject, item: Subject, quantity: Decimal)
predicate Hold(item: Subject)
predicate Note(text: Subject)

invariant positive_stock:
    StockItem(_, _, q) implies q >= 0

transformation allocate(allocation, line, item, quantity, note):
    require positive: quantity > 0
    bind found_item: StockItem(item, item_part, available)
    bind found_line: SalesLine(line, line_part, _)
    require same_part: item_part = line_part or PartDescendsFrom(item_part, line_part)
    let left = available - sum(q | Allocation(_, _, item, q))
    require enough: quantity <= left
    require not Hold(item)
    require quiet: not Note(#lock)
    if note = #urgent:
        require urgent_ok: Part(item_part)
    else:
        require routine_ok: SalesLine(line, _, quantity)
    admit Allocation(allocation, line, item, quantity)

transformation release(items, line):
    for item in items:
        require each_held: Hold(item)
    for SalesLine(line, part, _):
        require each_part: Part(part)
    admit Note(#released)

transformation shadow(q, item):
    require summed: sum(q | Allocation(_, _, item, q)) > 0
    admit Note(#shadow)
";

fn related(transformation: &str, rule: &str) -> Option<Vec<String>> {
    let program = morpholog_surface::parse_program(PROGRAMME).expect("fixture parses");
    program.validate().expect("fixture validates");
    let t = program
        .transformations
        .iter()
        .find(|t| t.name.as_str() == transformation)
        .expect("transformation");
    related_parameters(t, rule)
}

fn names(names: &[&str]) -> Option<Vec<String>> {
    Some(names.iter().map(ToString::to_string).collect())
}

#[test]
fn a_gate_names_the_parameters_it_reads_directly() {
    assert_eq!(related("allocate", "positive"), names(&["quantity"]));
}

#[test]
fn a_gate_names_parameters_it_reads_through_bindings() {
    // `same_part` reads only locals, each bound by a pattern keyed on one
    // parameter: both parameters, not the locals.
    assert_eq!(related("allocate", "same_part"), names(&["line", "item"]));
    // Through a `let` over a bound local and a sum keyed on `item`.
    assert_eq!(related("allocate", "enough"), names(&["item", "quantity"]));
}

#[test]
fn a_failed_bind_names_what_it_reads_before_it_binds() {
    assert_eq!(related("allocate", "found_item"), names(&["item"]));
}

#[test]
fn a_gate_over_the_state_alone_names_none() {
    assert_eq!(related("allocate", "quiet"), names(&[]));
}

#[test]
fn a_gate_in_a_branch_names_what_it_reads_and_not_the_test() {
    // `note` decides which branch runs; neither gate reads it. The first
    // reads `item_part`, bound from `item` before the `if`.
    assert_eq!(related("allocate", "urgent_ok"), names(&["item"]));
    assert_eq!(
        related("allocate", "routine_ok"),
        names(&["line", "quantity"])
    );
}

#[test]
fn a_loop_variable_carries_what_its_source_reads() {
    assert_eq!(related("release", "each_held"), names(&["items"]));
    assert_eq!(related("release", "each_part"), names(&["line"]));
}

#[test]
fn a_sum_variable_named_like_a_parameter_still_reads_it() {
    // Inside the sum's body `q` is already bound, so the pattern tests the
    // parameter: the gate reads `q` and `item`.
    assert_eq!(related("shadow", "summed"), names(&["q", "item"]));
}

#[test]
fn an_invariant_or_an_unnamed_gate_has_no_entry() {
    assert_eq!(related("allocate", "positive_stock"), None);
    assert_eq!(related("allocate", "no_such_rule"), None);
}
