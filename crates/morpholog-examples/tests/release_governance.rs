//! Integration tests for the release-governance example
//! (`examples/16_release_governance/`) - the checklist rules proven
//! uncommittable when broken, through the full `propose()` path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, subj};
use morpholog_core::State;
use morpholog_examples::release_governance;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&release_governance::program()))
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
fn a_complete_release_announces() {
    ex().must_accept(
        &release_governance::announce(),
        vec![subj("v0_0_8")],
        ready_to_announce(),
    );
}

#[test]
fn tagging_an_ungated_commit_is_refused() {
    ex().must_reject(
        &release_governance::tag_release(),
        vec![subj("v0_0_8"), subj("commit_nobody_gated")],
        &State::default(),
    );
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
    // one-commit-per-version uniqueness law, by name.
    let reason = e.must_reject(
        &release_governance::tag_release(),
        vec![subj("v0_0_8"), subj("commit_other")],
        &state,
    );
    assert!(
        reason.to_string().contains("tagged_unique_by_version"),
        "expected the generated uniqueness invariant, got: {reason}"
    );
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
    assert!(
        reason
            .to_string()
            .contains("every_declared_platform_has_its_asset"),
        "the refusal names the gate: {reason}"
    );
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
    assert!(
        reason.to_string().contains("platform_is_in_the_matrix"),
        "the refusal names the gate: {reason}"
    );
}
