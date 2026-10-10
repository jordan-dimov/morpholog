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
