# Biometric identification oversight: EU AI Act Article 12 as admission law

An authority asks, two years later: *on what basis was this person identified,
who verified it, were they authorised and trained that day, and which version
of the model was in service?* That should be one lookup, not a forensic dig
across four systems and a log archive.

From 2 August 2026, Regulation (EU) 2024/1689 (the EU AI Act) makes that
question legally loaded. High-risk AI systems - and remote biometric
identification is the statute's own first example (Annex III, point 1(a)) -
must keep automatic records of every use, and no action may be taken on an
identification unless at least two people have separately verified it.
Non-compliance with provider or deployer obligations carries fines up to
EUR 15 million or 3% of worldwide annual turnover (Article 99(4)).

The usual answer is a logging pipeline beside the AI system and a policy
document asking everyone to follow the rules. This example demonstrates the
other answer: the statutory record as **governed state**, where an
inadmissible decision record cannot be committed at all, and the Article 12
log is not a subsystem bolted on - the log IS the governed record, read back.

## The sentence this example teaches

> The AI system's output enters the record as a claim with no standing.
> Standing is granted only by verification under live, revocable human
> authority. Decisions gate on standing, never on the raw output. Revoking an
> overseer's authority stops future verifications and leaves every past
> decision a valid record of what was decided then.

## The statute, clause by clause

Every rule in [`biometric_oversight.morph`](biometric_oversight.morph) traces
to a clause of the final text (verified against Regulation (EU) 2024/1689 as
published; article numbers are from the final regulation, not the draft):

| Statute | Requirement | Rule in the model |
|---|---|---|
| Art. 12(1) | High-risk systems technically allow automatic recording of events over their lifetime | The substrate itself: every transformation commits a claim-and-audit record or nothing |
| Art. 12(3)(a) | Record the period of each use (start and end date and time) | `UseStarted` / `UseEnded` claims; `UsePeriod` derived (period and exact length, computed, never stored) |
| Art. 12(3)(b) | Record the reference database checked | `reference_db` on `UseStarted` |
| Art. 12(3)(c) | Record the input data for which the search led to a match | `input_ref` on `MatchRecorded` |
| Art. 12(3)(d) | Record the identity of the natural persons who verified the results | `MatchVerified(match, verifier, verified_at)` - the verifier is the proposing actor, recorded in the claim and the audit row |
| Art. 14(5) | No action or decision on an identification unless separately verified by at least two natural persons | The `decide_on_identification` gate and the `decision_rests_on_two_distinct_prior_verifications` invariant - two verification records with distinct verifiers, **both at or before the decision**, or the decision cannot commit |
| Art. 26(2) | Deployers assign oversight to natural persons with competence, training and authority | `OversightAssigned`, granted and revoked by `assign_oversight` / `revoke_oversight`; consulted as a gate at each verification |
| Art. 19(1), 26(6) | Providers and deployers keep logs at least six months | No machinery needed: the substrate never deletes, so any retention minimum is trivially exceeded |
| Art. 86(1) | An affected person may demand a clear and meaningful explanation of the decision | One as-of lookup: the decision, its match, the input reference, both verifier identities, the version in service, and the oversight assignments in force - all at the decision's transition |

## The refusals that carry the argument

Each beat of the walkthrough (typed out in
`crates/morpholog-examples/tests/biometric_identification_oversight.rs`) is a
proposal the runtime refuses, with the reason named:

1. **A use cannot start under a model version not in service.** An unassessed
   version cannot put anything on the record - not flagged later, never
   admitted.
2. **A decision on one verification is refused.** Ask `morpholog explain` and
   the answer names the missing claim in the statute's own terms: a
   verification by a second person.
3. **The same overseer verifying twice is one voice, not two.** The second
   verification is itself refused; the two-person rule cannot be satisfied
   single-handedly.
4. **A decision cannot be dated before the verifications it rests on.** Two
   verification records existing by the time someone files the decision is not
   the statute's "verification before action" - both must *precede* the
   decision instant, or it is refused. The subtlest gap, and exactly the kind
   admission law closes that a dashboard reports on too late.
5. **A revoked overseer cannot verify** - and the decision they helped verify
   last month stands untouched. Whether a decision was allowed is settled
   when it is made. As-of replay shows the authority held then.
6. **A use period cannot be closed earlier than a match it already
   produced.** Backdating the end of a use to exclude an awkward match is not
   forbidden by policy; it is uncommittable.

Note who proposes `record_match`: the AI system itself, as the actor, and
`require actor = system` enforces it - a match attributed to this system
genuinely originated from it, not from an analyst typing one in. A machine
actor passes the same gates as a human one, and here its identity is part of
what makes the record admissible. The thing producing candidates does not have
to be trusted to behave; it only gets to *propose*, and admissibility - down
to who proposed - is enforced outside it.

There is deliberately no clock in the model. Every timestamp is supplied by
the proposer and judged by the gates; nothing reads "now" from the machine it
runs on. Replay the record next year, in front of a regulator, and every
admission decision comes out the same.

## What this example deliberately does not claim

- It does not make a deployer "AI Act compliant". Conformity assessment, risk
  management, data governance, and the rest of the regulation are out of
  scope; this demonstrates what Article 12's record-keeping and Article
  14(5)'s verification discipline look like when they are admission rules.
- Article 14(5) itself carries an exception: for law enforcement, migration,
  border control and asylum, the two-person requirement can be disapplied
  where Union or national law considers it disproportionate. The model
  expresses the rule as it applies when not disapplied; whether it applies is
  a legal question, not a modelling one.
- The matcher's internals - confidence scores, thresholds, embeddings - are
  outside the boundary. The runtime governs what may enter the record, not
  how the model computed its candidate.
- Hash-chained or blockchain-style logging solves a different problem:
  tamper-evidence, proof that nobody altered the record after the fact. This
  example demonstrates the layer above - invalid records were never
  admissible in the first place. The two compose; neither replaces the other.

## What this example forced

Nothing - and that is the headline, not a footnote. Authority grant/revoke is
the approval-controls example's shape; admission-time validity windows are
the clinical-trial example's; standing granted by verification is verified
revenue's; exact instants and durations are laytime's. Four shipped patterns
met a statute, and the language did not move. The only new surface in this
example's PR is read-side tooling (`morpholog inspect controls`), which
derives from the parsed programme and adds no kernel primitive.

## Running it

```bash
morpholog check examples/13_biometric_identification_oversight/biometric_oversight.morph
morpholog inspect controls examples/13_biometric_identification_oversight/biometric_oversight.morph
morpholog inspect guarantees examples/13_biometric_identification_oversight/biometric_oversight.morph
```
