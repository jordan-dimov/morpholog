# Release governance: the project's own checklist as law

Every project has release rules - gate before tag, one commit per
version, every platform's download before the announcement - and they
usually live in a checklist that works until the day someone is tired.
This example turns that checklist into admission law: a release step
out of order is not caught in review, it refuses to commit.

It is also Morpholog governing itself. Each rule here has a near-miss
in this repository's own history, and the programme is used by hand
(one `morpholog propose` per step) during real releases of this
project. No automation: the first stage proves the law is worth
having; plumbing GitHub events through the outbox pattern is a later
stage, if living under it earns it.

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

```
morpholog check examples/16_release_governance/release_governance.morph
morpholog inspect controls examples/16_release_governance/release_governance.morph
```

## Deliberately not covered

The pull-request lifecycle (review-fix and land-together rules) - that
is a second programme wanting live GitHub events, not hand admission.
Migration and floor claims wait until a release that ships a migration
forces their shape.
