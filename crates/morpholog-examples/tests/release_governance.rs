//! Integration tests for the release-governance example
//! (`examples/16_release_governance/`) - each checklist rule proven
//! uncommittable when broken, through the full `propose()` path, with
//! refusals matched structurally on the rule's name (never parsed out
//! of display prose).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, subj};
use morpholog_core::{Outcome, RejectionReason, State};
use morpholog_examples::release_governance;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&release_governance::program()))
}

/// The refusal is a `require` gate with exactly this name.
fn expect_gate(reason: &RejectionReason, expected: &str) {
    match reason {
        RejectionReason::Require { name: Some(n), .. } => {
            assert_eq!(n.as_str(), expected, "a different gate refused: {reason}")
        }
        other => panic!("expected the `{expected}` gate, got: {other}"),
    }
}

/// A complete release up to (not including) the announcement: matrix
/// of two platforms, gate, tag, both assets, changelog.
fn ready_to_announce() -> State {
    let e = ex();
    let mut state = State::default();
    for platform in ["linux_x86_64", "linux_arm64"] {
        state = e.must_accept(
            &release_governance::declare_platform(),
            vec![subj(platform)],
            state,
        );
    }
    state = e.must_accept(
        &release_governance::record_gate(),
        vec![subj("v0_0_8"), subj("commit_abc")],
        state,
    );
    state = e.must_accept(
        &release_governance::tag_release(),
        vec![subj("v0_0_8"), subj("commit_abc")],
        state,
    );
    for platform in ["linux_x86_64", "linux_arm64"] {
        state = e.must_accept(
            &release_governance::publish_asset(),
            vec![subj("v0_0_8"), subj(platform)],
            state,
        );
    }
    e.must_accept(
        &release_governance::record_changelog(),
        vec![subj("v0_0_8")],
        state,
    )
}

#[test]
fn a_complete_release_announces_and_emits_exactly_one_intent() {
    let outcome = ex()
        .propose(
            &release_governance::announce(),
            vec![subj("v0_0_8")],
            &ready_to_announce(),
        )
        .unwrap();
    let Outcome::Accepted {
        emitted_intents, ..
    } = outcome
    else {
        panic!("a complete release must announce");
    };
    // The intent is the eventual integration point (the later stage
    // drives the real publish from it), so its exact payload is
    // load-bearing: one intent, this name, this version.
    assert_eq!(emitted_intents.len(), 1);
    assert_eq!(emitted_intents[0].name, "ReleaseAnnounced");
    assert_eq!(emitted_intents[0].args, vec![subj("v0_0_8")]);
}

#[test]
fn announcing_the_same_version_twice_is_refused() {
    let e = ex();
    let state = e.must_accept(
        &release_governance::announce(),
        vec![subj("v0_0_8")],
        ready_to_announce(),
    );
    // The replay guard: admitting `Announced` again would be a no-op,
    // but the intent would emit a second time - so the second act is
    // refused outright, by name.
    let reason = e.must_reject(
        &release_governance::announce(),
        vec![subj("v0_0_8")],
        &state,
    );
    expect_gate(&reason, "release_not_already_announced");
}

#[test]
fn tagging_an_ungated_commit_is_refused() {
    let reason = ex().must_reject(
        &release_governance::tag_release(),
        vec![subj("v0_0_8"), subj("commit_nobody_gated")],
        &State::default(),
    );
    expect_gate(&reason, "gate_ran_green_at_this_commit");
}

#[test]
fn retagging_a_version_at_a_second_commit_is_refused() {
    let e = ex();
    let mut state = ready_to_announce();
    state = e.must_accept(
        &release_governance::record_gate(),
        vec![subj("v0_0_8"), subj("commit_other")],
        state,
    );
    // The gate passes (commit_other ran green) - the refusal is the
    // one-commit-per-version uniqueness law.
    let reason = e.must_reject(
        &release_governance::tag_release(),
        vec![subj("v0_0_8"), subj("commit_other")],
        &state,
    );
    match &reason {
        RejectionReason::Invariant { name, .. } => {
            assert_eq!(name.as_str(), "tagged_unique_by_version")
        }
        other => panic!("expected the generated uniqueness invariant, got: {other}"),
    }
}

#[test]
fn publishing_an_asset_before_the_version_is_tagged_is_refused() {
    let e = ex();
    let state = e.must_accept(
        &release_governance::declare_platform(),
        vec![subj("linux_x86_64")],
        State::default(),
    );
    let reason = e.must_reject(
        &release_governance::publish_asset(),
        vec![subj("v0_0_8"), subj("linux_x86_64")],
        &state,
    );
    expect_gate(&reason, "version_is_tagged");
}

#[test]
fn publishing_an_asset_for_an_undeclared_platform_is_refused() {
    let e = ex();
    let mut state = State::default();
    state = e.must_accept(
        &release_governance::record_gate(),
        vec![subj("v0_0_8"), subj("commit_abc")],
        state,
    );
    state = e.must_accept(
        &release_governance::tag_release(),
        vec![subj("v0_0_8"), subj("commit_abc")],
        state,
    );
    let reason = e.must_reject(
        &release_governance::publish_asset(),
        vec![subj("v0_0_8"), subj("windows_x86_64")],
        &state,
    );
    expect_gate(&reason, "platform_is_in_the_matrix");
}

#[test]
fn announcing_an_untagged_version_is_refused() {
    let reason = ex().must_reject(
        &release_governance::announce(),
        vec![subj("v9_9_9")],
        &ready_to_announce(),
    );
    expect_gate(&reason, "version_is_tagged");
}

#[test]
fn announcing_without_a_changelog_is_refused() {
    let e = ex();
    let mut state = State::default();
    state = e.must_accept(
        &release_governance::record_gate(),
        vec![subj("v0_0_8"), subj("commit_abc")],
        state,
    );
    state = e.must_accept(
        &release_governance::tag_release(),
        vec![subj("v0_0_8"), subj("commit_abc")],
        state,
    );
    // No platforms declared, so the completeness gate is vacuously
    // satisfied; the changelog gate is what refuses.
    let reason = e.must_reject(
        &release_governance::announce(),
        vec![subj("v0_0_8")],
        &state,
    );
    expect_gate(&reason, "changelog_written");
}

#[test]
fn announcing_with_a_platform_missing_its_asset_is_refused() {
    let e = ex();
    // Grow the matrix after the assets were published: the third
    // platform has no asset, so the announcement gate refuses.
    let state = e.must_accept(
        &release_governance::declare_platform(),
        vec![subj("macos_arm64")],
        ready_to_announce(),
    );
    let reason = e.must_reject(
        &release_governance::announce(),
        vec![subj("v0_0_8")],
        &state,
    );
    expect_gate(&reason, "every_declared_platform_has_its_asset");
}

#[test]
fn a_platform_declared_later_does_not_invalidate_a_past_announcement() {
    let e = ex();
    let state = e.must_accept(
        &release_governance::announce(),
        vec![subj("v0_0_8")],
        ready_to_announce(),
    );
    // The gate-not-invariant doctrine, proven: growing the matrix
    // AFTER announcing commits cleanly - the past announcement stays
    // lawful, exactly as a closed period keeps its entries.
    e.must_accept(
        &release_governance::declare_platform(),
        vec![subj("macos_arm64")],
        state,
    );
}
