//! `docs/surface-syntax.md` shows every surface form once, and each block
//! on it must be a programme the parser accepts, the validator passes and
//! the lints leave alone: a sheet that drifts from the parser teaches the
//! wrong spelling.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::lints;
use morpholog_surface::parse_program;
use morpholog_test_support::prepare;

fn sheet_blocks() -> Vec<(usize, String)> {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/surface-syntax.md");
    let text = std::fs::read_to_string(&path).expect("docs/surface-syntax.md");
    let mut blocks = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (number, line) in text.lines().enumerate() {
        match (&mut current, line.trim_end()) {
            (None, "```morph") => current = Some((number + 1, String::new())),
            (Some(_), "```") => blocks.push(current.take().unwrap()),
            (Some((_, body)), _) => {
                body.push_str(line);
                body.push('\n');
            }
            (None, _) => {}
        }
    }
    assert!(current.is_none(), "an unclosed block");
    blocks
}

#[test]
fn every_block_on_the_sheet_parses_validates_and_is_lint_clean() {
    let blocks = sheet_blocks();
    assert!(blocks.len() >= 8, "anti-vacuity: {} blocks", blocks.len());
    for (line, source) in blocks {
        let program = parse_program(&source)
            .unwrap_or_else(|e| panic!("the block at line {line} does not parse: {e:?}"));
        program
            .validate()
            .unwrap_or_else(|e| panic!("the block at line {line} does not validate: {e:?}"));
        let found = lints(&prepare(&program));
        assert!(
            found.is_empty(),
            "the block at line {line} ({}) is not lint-clean: {found:?}",
            program.name
        );
    }
}

// ============================================================
// What the prose claims, proposed through the kernel
// ============================================================

use morpholog_core::{Outcome, State, Transition};
use morpholog_test_support::{dec, propose, subj, subjects};

fn block_named(name: &str) -> morpholog_core::Program {
    let (_, source) = sheet_blocks()
        .into_iter()
        .find(|(_, s)| s.contains(&format!("program {name}\n")))
        .unwrap_or_else(|| panic!("no block for programme {name}"));
    parse_program(&source).expect("parses")
}

fn act(
    program: &morpholog_core::Program,
    name: &str,
    args: Vec<morpholog_core::EvalValue>,
    state: &State,
) -> Outcome {
    let t = Transition {
        transformation_name: name.into(),
        args,
        actor: "sheet".into(),
        decided_at: None,
    };
    propose(program, &t, state, &mut subjects(["s1", "s2"])).expect("no kernel error")
}

fn accepted(outcome: Outcome) -> State {
    match outcome {
        Outcome::Accepted {
            candidate_state, ..
        } => candidate_state,
        Outcome::Rejected { reason } => panic!("expected acceptance, got {reason}"),
    }
}

/// A limit with no exposure yet is an ordinary first state: the guarded
/// extremum lets it in, where an unguarded one would raise.
#[test]
fn a_limit_with_no_exposures_is_admissible() {
    let p = block_named("values");
    let state = accepted(act(
        &p,
        "set_limit",
        vec![subj("acct"), dec(100)],
        &State::default(),
    ));
    let state = accepted(act(
        &p,
        "expose",
        vec![subj("pos1"), subj("acct"), dec(60)],
        &state,
    ));
    let refused = act(
        &p,
        "expose",
        vec![subj("pos2"), subj("acct"), dec(150)],
        &state,
    );
    assert!(
        matches!(refused, Outcome::Rejected { .. }),
        "the largest position is now over the cap"
    );
}

/// Entries posted while a period was open do not stop it closing: the
/// check lives in the gate of `post`, not in a rule over history.
#[test]
fn a_period_with_entries_still_closes() {
    let p = block_named("rules");
    let state = accepted(act(&p, "open_period", vec![subj("p1")], &State::default()));
    let state = accepted(act(
        &p,
        "post",
        vec![subj("e1"), subj("p1"), dec(10)],
        &state,
    ));
    let state = accepted(act(&p, "close_period", vec![subj("p1")], &state));
    let refused = act(&p, "post", vec![subj("e2"), subj("p1"), dec(10)], &state);
    assert!(
        matches!(refused, Outcome::Rejected { .. }),
        "nothing posts into the closed period from here on"
    );
}

/// A claim is replaced, never edited: admitting a second balance under
/// the same key is refused, and the replacement act retracts first.
#[test]
fn a_balance_is_replaced_by_retracting_the_old_one() {
    let p = block_named("disciplines");
    let state = accepted(act(
        &p,
        "rebalance",
        vec![subj("acct"), dec(0), dec(100)],
        &State::from_claims(vec![morpholog_test_support::claim_instance(
            "Balance",
            &[subj("acct"), dec(0)],
        )]),
    ));
    let state = accepted(act(
        &p,
        "rebalance",
        vec![subj("acct"), dec(100), dec(250)],
        &state,
    ));
    let stale = act(
        &p,
        "rebalance",
        vec![subj("acct"), dec(100), dec(300)],
        &state,
    );
    assert!(
        matches!(stale, Outcome::Rejected { .. }),
        "the old amount is gone, so the stale replacement is refused"
    );
}

/// A large entry is posted first and decided after: the rule asks that an
/// outcome has its entry, never that an entry has its outcome already, so
/// the two acts do not wait on each other.
#[test]
fn a_large_entry_is_posted_then_decided() {
    let p = block_named("rules");
    let state = accepted(act(&p, "open_period", vec![subj("p1")], &State::default()));
    let state = accepted(act(
        &p,
        "post",
        vec![subj("big"), subj("p1"), dec(20_000)],
        &state,
    ));
    let state = accepted(act(
        &p,
        "decide",
        vec![subj("big"), subj("accepted")],
        &state,
    ));
    let orphan = act(&p, "decide", vec![subj("nobody"), subj("accepted")], &state);
    assert!(
        matches!(orphan, Outcome::Rejected { .. }),
        "an outcome for an entry that does not exist is refused"
    );
}

/// Consent given after the day asked about is no consent for that day.
#[test]
fn consent_counts_from_the_day_it_was_given() {
    use morpholog_test_support::date;
    let p = block_named("definitions_and_reads");
    let state = accepted(act(
        &p,
        "issue_form",
        vec![subj("f1"), date("2026-01-01"), date("2026-12-31")],
        &State::default(),
    ));
    let state = accepted(act(
        &p,
        "consent",
        vec![subj("pat"), subj("f1"), date("2026-06-15")],
        &state,
    ));
    let early = act(&p, "enrol", vec![subj("pat"), date("2026-03-01")], &state);
    assert!(
        matches!(early, Outcome::Rejected { .. }),
        "consent did not exist on 1 March"
    );
    accepted(act(
        &p,
        "enrol",
        vec![subj("pat"), date("2026-07-01")],
        &state,
    ));
}

fn refused_by(outcome: Outcome, rule: &str) {
    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason.to_string().contains(&format!("`{rule}`")),
            "expected `{rule}` to refuse, got {reason}"
        ),
        Outcome::Accepted { .. } => panic!("expected `{rule}` to refuse, but it was admitted"),
    }
}

/// The sibling standing in for an absent field goes with its parent: one
/// pattern retract on the shared field removes both, and an act that
/// removes only the parent is refused by the rule that ties them.
#[test]
fn a_sibling_for_an_absent_field_goes_with_its_parent() {
    let (_, source) = sheet_blocks()
        .into_iter()
        .find(|(_, s)| s.contains("program optional_field\n"))
        .expect("the optional-field block");
    let forgetful = format!(
        "{source}\ntransformation cancel_allocations_only(line):\n    retract Allocation(line: line, ..)\n"
    );
    let p = parse_program(&forgetful).expect("parses");
    let state = accepted(act(
        &p,
        "allocate",
        vec![subj("a1"), subj("l1"), dec(2)],
        &State::default(),
    ));
    let state = accepted(act(
        &p,
        "allocate",
        vec![subj("a2"), subj("l1"), dec(3)],
        &state,
    ));
    let state = accepted(act(&p, "ship", vec![subj("a1"), subj("s1")], &state));
    refused_by(
        act(&p, "cancel_allocations_only", vec![subj("l1")], &state),
        "a_shipment_names_a_live_allocation",
    );
    let state = accepted(act(&p, "cancel_line", vec![subj("l1")], &state));
    assert_eq!(state.len(), 0, "the cascade left claims behind");
}

/// A closure the host lists is held to the tree: a complete one is
/// admitted, and one missing a parent or an ancestor, naming a wrong one,
/// or closing a cycle, is refused, each by the rule it breaks.
#[test]
fn a_host_kept_closure_is_held_to_the_tree() {
    use morpholog_test_support::coll;
    let (_, source) = sheet_blocks()
        .into_iter()
        .find(|(_, s)| s.contains("program variant_tree\n"))
        .expect("the variant-tree block");
    let p = parse_program(&format!(
        "{source}\ntransformation record_parent_only(part, parent):\n    admit VariantOf(part, parent)\n\
         transformation record_pair(x, y):\n    admit VariantOf(x, y)\n    admit VariantOf(y, x)\n\
         \x20   admit DescendsFrom(x, y)\n    admit DescendsFrom(y, x)\n\
         \x20   admit DescendsFrom(x, x)\n    admit DescendsFrom(y, y)\n"
    ))
    .expect("parses");
    let record = |part: &str, parent: &str, ancestors: &[&str], state: &State| {
        act(
            &p,
            "record_variant",
            vec![
                subj(part),
                subj(parent),
                coll(ancestors.iter().map(|a| subj(a)).collect()),
            ],
            state,
        )
    };
    refused_by(
        act(
            &p,
            "record_parent_only",
            vec![subj("b"), subj("a")],
            &State::default(),
        ),
        "a_variant_descends_from_its_parent",
    );
    let state = accepted(record("b", "a", &[], &State::default()));
    refused_by(record("c", "b", &[], &state), "descent_is_transitive");
    refused_by(
        record("c", "b", &["a", "z"], &state),
        "descent_follows_the_tree",
    );
    let state = accepted(record("c", "b", &["a"], &state));
    accepted(record("d", "c", &["b", "a"], &state));
    refused_by(
        record("e", "e", &[], &State::default()),
        "no_part_descends_from_itself",
    );
    refused_by(
        act(
            &p,
            "record_pair",
            vec![subj("x"), subj("y")],
            &State::default(),
        ),
        "no_part_descends_from_itself",
    );
}
