# Release governance: the project's own checklist as a governed register

Every project has release rules - gate before tag, one commit per
version, every platform's download before the announcement - and they
usually live in a checklist that works until the day someone is tired.
This example turns that checklist into a governed release register:
an inconsistent release record refuses to commit.

The enforcement boundary, stated honestly: the register governs the
RECORD of a release, not yet the release operation. A git tag or a
GitHub release made without proposing anything here is not prevented -
the record would simply be missing, and `inspect claims` would show
it. Moving the operation itself behind the accepted transition (the
announcement intent driving the actual publish through the outbox) is
the later stage, taken only if living under the register for a real
release cycle earns it.

Each rule here has a near-miss in this repository's own history, and
the register is kept by hand (one `morpholog propose` per step)
during real releases of this project, starting with the next one.

## The programme at a glance

Claims: the platform matrix (`PlatformDeclared`), green gate runs
(`GateGreen`, append only), tags (`Tagged`, one commit per version,
append only), published downloads (`AssetPublished`), changelog
entries, and announcements (`Announced`, append only).

Invariants: a tag exists only at a gated commit; an announcement never
exists without its tag and changelog.

Transformations: one per checklist step, ending in `announce` - whose
gate demands a download for every declared platform. Why completeness
is a gate rather than an invariant is the example's teaching moment;
the `.morph` walks through it.

Forced no kernel or surface addition: the process domain composes
entirely from existing primitives, which is the point.

## Run it

Static views:

```
morpholog check examples/16_release_governance/release_governance.morph
morpholog inspect controls examples/16_release_governance/release_governance.morph
```

One release, kept in the register end to end (a disposable database;
`MORPH` abbreviates the `.morph` path, `DB` the `--database-url`):

```
morpholog init --database-url $DB
# One declaration per platform the release channel builds for. The
# announce gate below demands an asset for every one of them, so this
# is the step that decides what "complete" means for this release.
for p in linux_x86_64 linux_arm64 macos_arm64; do
  morpholog propose $MORPH declare_platform --actor releaser \
      --args-named "{\"platform\": \"$p\"}" --database-url $DB
done
morpholog propose $MORPH record_gate --actor releaser \
    --args-named '{"version": "v0_0_8", "commit": "commit_abc"}' --database-url $DB
morpholog propose $MORPH tag_release --actor releaser \
    --args-named '{"version": "v0_0_8", "commit": "commit_abc"}' --database-url $DB
for p in linux_x86_64 linux_arm64 macos_arm64; do
  morpholog propose $MORPH publish_asset --actor releaser \
      --args-named "{\"version\": \"v0_0_8\", \"platform\": \"$p\"}" --database-url $DB
done
morpholog propose $MORPH record_changelog --actor releaser \
    --args-named '{"version": "v0_0_8"}' --database-url $DB
morpholog propose $MORPH announce --actor releaser \
    --args-named '{"version": "v0_0_8"}' --database-url $DB
morpholog inspect claims $MORPH --database-url $DB
morpholog audit tail --database-url $DB
morpholog inspect outbox --database-url $DB
```

Propose any step out of order and the refusal names the gate that
turned it away; propose `announce` twice and the replay guard refuses
the second. Leave one platform's `publish_asset` out and `announce`
refuses too - which is the rule doing the work it was written for, now
that the channel builds for more than one platform.

## Deliberately not covered

The pull-request lifecycle (review-fix and land-together rules) - that
is a second programme wanting live GitHub events, not hand admission.
Migration and floor claims wait until a release that ships a migration
forces their shape. The platform matrix is monotonic in this first
version: platforms may be added but not retired or temporarily
excluded. And `AssetPublished` is a checklist assertion, not evidence
tying the announcement to particular bytes - a later version can carry
the artifact name and digest, turning "there was an asset" into "these
exact bytes were declared as the asset" with no new language
machinery.
