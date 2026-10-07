# Biometric identification oversight

**An AI system's identification counts for nothing until two different,
currently authorised people have verified it - and every use leaves the
record the EU AI Act asks for.** The AI system only proposes; whether its
match may be acted on is decided outside it.

## Why this matters

From 2 August 2026, Regulation (EU) 2024/1689 (the EU AI Act) applies to
high-risk AI systems, and remote biometric identification is the statute's
own first example (Annex III, point 1(a)). Such a system must keep automatic
records of every use (Article 12), and no action may be taken on an
identification unless at least two people have separately verified it
(Article 14(5)). Non-compliance with provider or deployer obligations
carries fines up to EUR 15 million or 3% of worldwide annual turnover
(Article 99(4)).

The usual answer is a logging pipeline beside the AI system and a policy
document asking everyone to follow the rules. Here the statutory record is
the governed record itself: a decision the statute forbids cannot be
committed at all, and the Article 12 log is that record, read back.

## What Morpholog enforces

- **The AI's match carries no weight on its own.** It enters the record
  with no standing to be acted on.
- **Two different people, verifying first.** A decision needs verifications
  by two distinct overseers, both made before the decision.
- **Live, revocable authority.** Only someone currently assigned oversight
  may verify. Revoking that assignment stops their future verifications and
  leaves every past decision a valid record of what was decided then.
- **A name only its owner can use.** A verifier's name, once restricted,
  can be asserted only by a database login authorised for it, so one
  operator cannot act as both verifiers. The deployer, who hands out
  oversight, is restricted the same way.
- **The match came from the system.** Only the AI system itself may record
  a match attributed to it.

## What it refuses

Each beat of the walkthrough (typed out in
`crates/morpholog-examples/tests/biometric_identification_oversight.rs`) is a
proposal the runtime refuses, with the reason named:

1. **A use cannot start under a model version not in service.** An unassessed
   version cannot put anything on the record - not flagged later, never
   admitted.
2. **A decision before any verification is refused, and explains itself.** Ask
   `morpholog explain` and the answer names the failing gate and the
   directly-missing claim in the statute's own terms - a `MatchVerified`,
   with `verify_match` as the transformation that supplies it. (Once one
   verification exists, the gate still refuses a decision - a second, distinct
   verifier is needed - but the gap is now distinctness rather than an absent
   claim, which the one-hop engine names as the failing gate without a
   missing-claim checklist. The deeper why-not is a deferred tier.)
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

## What you can show afterwards

Asked about any decision, the record answers in one as-of lookup: the
decision, its match, the input it was matched on, both verifiers, the model
version in service, and the oversight assignments in force at that moment.
That is the explanation an affected person may demand under Article 86(1).

There is deliberately no clock in the model. Every timestamp is supplied by
the proposer and judged by the gates; nothing reads "now" from the machine it
runs on. Replay the record next year, in front of a regulator, and every
admission decision comes out the same.

## Where it fits

The matching system keeps doing the matching: confidence scores,
thresholds and embeddings stay inside it. It proposes each match through
Morpholog as itself, and the overseers' verifications and decisions go
through Morpholog from their own applications. Morpholog is not a logging
pipeline beside the AI system; it is where a match becomes something that
may be acted on.

## The statute, clause by clause

Every rule in [`biometric_oversight.morph`](biometric_oversight.morph) traces
to a clause of the final text, verified against the Official Journal
(Regulation (EU) 2024/1689, OJ L, 12.7.2024; article numbers are from the
final regulation, not the draft):

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
- Restricting who may assert a verifier's name proves that a proposal
  arrived over a connection authorised for that name. It does not prove the
  person was present, or consented, or looked at anything. And it binds only
  callers who reach the record through Morpholog: the runtime's own database
  role can write claims and audit rows directly, so two verifier identities
  are genuinely distinct only when the two applications and their credentials
  are genuinely separate.
- Hash-chained or blockchain-style logging solves a different problem:
  tamper-evidence, proof that nobody altered the record after the fact. This
  example demonstrates the layer above - invalid records were never
  admissible in the first place. The two compose; neither replaces the other.

## Running it

```bash
morpholog check examples/13_biometric_identification_oversight/biometric_oversight.morph
morpholog inspect controls examples/13_biometric_identification_oversight/biometric_oversight.morph
morpholog inspect guarantees examples/13_biometric_identification_oversight/biometric_oversight.morph
```
