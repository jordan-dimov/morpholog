# Clinical Trial Enrolment

A regulator is reviewing a Phase II trial three years after data lock. One question on the inspection list: *was participant P001 legitimately randomised into this trial on 12 March 2026 - under the protocol version, consent form, investigator delegation and eligibility evidence that were valid on that day?* The answer should be one query against the trial's own records - not a forensic reconstruction across the eTMF, the IRB minutes, the delegation log and the central laboratory's expired-result archive.

This is the shape that ICH E6(R3), the current Good Clinical Practice guideline finalised at ICH Step 4 in January 2025, asks sponsors and investigators to be able to answer. The guideline puts heavy emphasis on protocol compliance, informed consent obtained on a version approved at the time, investigator delegation, audit trails, and data records that show *what was admitted, under what evidence in force then*. The clinical-trial industry already lives with the consequences of getting this wrong: a single misdated consent or a screening lab that expired the day before randomisation can void evidence for a participant or, in the worst cases, for an entire site.

This example is the smallest evidence regime that makes the regulator's question answerable by construction.

## The scenario

A Phase II trial, `trial_001`, runs from January through December 2026.

**Protocol version `proto_v1`** is ethics-approved and effective from **2026-01-01 to 2026-03-31** inclusive. **Consent form version `icf_v1`** is approved over the same window.

**Dr Smith** is delegated as an investigator on `trial_001` for the `randomise_participant` role from **2026-01-01 to 2026-12-31**.

The protocol defines one eligibility criterion - `creatinine_panel` - which must report `PASS` for the participant to be eligible.

Participant `P001` signs `icf_v1` on **2026-03-08**. A creatinine panel is run on **2026-03-09** and reports `PASS`; the result is valid through **2026-03-23** under the lab's stability window.

Dr Smith randomises `P001` on **2026-03-12**. Every validity gate holds on that date: protocol window covers it (inclusive), consent form window covers it, the consent signature precedes it, the delegation covers it, the assessment is unexpired, and there is no open important protocol deviation. The runtime admits the randomisation. `ParticipantRandomised(P001, trial_001, proto_v1, 2026-03-12, dr_smith)` is recorded; an outbox intent fires for the downstream IRT or eCRF system.

A week later, the sponsor amends the protocol. **`proto_v2`** is ethics-approved and effective from **2026-04-01**. The runtime admits the new protocol version alongside the old. The earlier `ParticipantRandomised` claim under `proto_v1` **remains valid** - validity was checked at admission, not as an eternal invariant. The doctrine is that closing a window must not retroactively invalidate decisions admitted under it.

A second participant, `P002`, is screened and consented in April. An investigator tries to randomise them on **2026-04-15** under `proto_v1`. The runtime evaluates the protocol window: `proto_v1` ended on 2026-03-31, three weeks earlier. **Rejected at admission.** Re-attempting under `proto_v2` with a valid `proto_v2` criterion and assessment admits. Both decisions are recorded against the rules in force on their respective dates.

The regulator three years later can ask:

- *Was P001's randomisation legitimate?* The `ParticipantRandomised` claim carries the protocol version, date and randomising actor. The protocol's effective window and the consent form's effective window can be replayed as-of that date; both included it.
- *Was the investigator authorised?* The `DelegatedInvestigator(dr_smith, trial_001, randomise_participant, ...)` claim was in force on that date.
- *Was the eligibility evidence current?* The `EligibilityAssessment` for P001 was assessed within the lab's stability window covering the randomisation date.
- *Did a later amendment invalidate this?* No. The amendment's window starts after the randomisation, and validity is an admission-time gate. The audit log shows both decisions, each under the rules then in force.

## The program

See [`clinical_trial_enrolment.morph`](clinical_trial_enrolment.morph) for the illustrative surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `Trial(trial_id)` | The trial container. Append-only. |
| `ProtocolVersion(trial_id, protocol_version, effective_from, effective_to)` | A protocol version and the inclusive `[from, to]` window over which it is in force. Append-only. |
| `ProtocolApprovedBy(protocol_version, ethics_committee, approval_id)` | The ethics approval record for a protocol version. |
| `ConsentFormVersion(trial_id, consent_form_version, effective_from, effective_to)` | A consent form version and its inclusive effective window. |
| `ConsentFormApprovedBy(consent_form_version, ethics_committee, approval_id)` | The ethics approval record for a consent form version. |
| `ParticipantScreened(participant_id, trial_id, screened_on)` | A point-in-time screening event. No window; downstream relevance is governed by what consents and assessments follow. |
| `InformedConsentObtained(participant_id, trial_id, consent_form_version, consented_on, obtained_by)` | Records the date, version, and obtaining actor for a participant's informed consent. |
| `EligibilityCriterion(protocol_version, criterion_id, required_result)` | A criterion that must report `required_result` for a participant to be eligible under this protocol version. |
| `EligibilityAssessment(participant_id, criterion_id, result, assessed_on, expires_on)` | A participant's assessment against a criterion, with assessment date and inclusive expiry. |
| `DelegatedInvestigator(actor, trial_id, role, effective_from, effective_to)` | An actor's delegated authority for a named role over an inclusive window. The load-bearing role-name constant for randomisation is `"randomise_participant"`. |
| `ImportantProtocolDeviationOpen(participant_id, trial_id, deviation_id)` | An open important protocol deviation against a participant. While admitted, the participant cannot be randomised. v0 has no closure transition. |
| `ParticipantRandomised(participant_id, trial_id, protocol_version, randomised_on, randomised_by)` | The audit-grade record of an admitted randomisation. Append-only; carries the protocol version and randomising actor in force at admission. |

### Invariants

| Invariant | What it pins |
| --- | --- |
| `at_most_one_protocol_window_per_version` | A given `(trial_id, protocol_version)` admits at most one effective window. Without it, a duplicate `ProtocolVersion` admission with conflicting windows could retroactively change whether an earlier randomisation was valid. |
| `at_most_one_consent_window_per_version` | Same shape for `ConsentFormVersion`. |
| `participant_randomised_once_per_trial` | Two `ParticipantRandomised` claims sharing `(participant_id, trial_id)` must agree on protocol version, date, and actor. Catches the "randomised twice under different protocols" footgun without making validity-window violations eternal. |

The absence of an invariant tying historical `ParticipantRandomised` claims to *currently* valid protocols, consent forms, or delegations is deliberate. Closing a window must not invalidate an admitted record - same require-vs-invariant doctrine as the verified-revenue and insurance-claim-settlement examples. Validity is checked at admission; the record stands.

### Transformations

| Transformation | Effect |
| --- | --- |
| `open_trial(trial_id)` | Opens a trial. |
| `approve_protocol_version(trial_id, protocol_version, effective_from, effective_to, ethics_committee, approval_id)` | Records an ethics-approved protocol version with its effective window. |
| `approve_consent_form_version(...)` | Same shape for consent form versions. |
| `delegate_investigator(investigator, trial_id, role, effective_from, effective_to)` | Records a delegated role for an investigator over an effective window. |
| `screen_participant(participant_id, trial_id, screened_on)` | Records a screening event. |
| `record_consent(participant_id, trial_id, consent_form_version, consented_on, obtained_by)` | Records informed consent for a specific form version. |
| `record_eligibility_criterion(protocol_version, criterion_id, required_result)` | Adds a criterion to a protocol version. |
| `record_eligibility_assessment(participant_id, criterion_id, result, assessed_on, expires_on)` | Records a participant's assessment against a criterion. |
| `open_important_protocol_deviation(participant_id, trial_id, deviation_id)` | Opens a deviation that blocks future randomisation of this participant. |
| `randomise_participant(participant_id, trial_id, protocol_version, randomised_on)` | The load-bearing transformation. **Declares no `actor` parameter.** The proposing actor flows through transition context as `$actor` and is consulted in the `DelegatedInvestigator` gate. Admits only if every validity-window check holds on `randomised_on` and the eligibility evidence matches the criterion's required result. |

## How to run it

```bash
# In-memory
cargo test -p morpholog-examples --test clinical_trial_enrolment

# Durable (PostgreSQL adapter)
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test clinical_trial_enrolment -- --test-threads=1
```

In-memory tests pin: happy-path admission, boundary equality at both window endpoints (protocol end, assessment expiry), each per-gate rejection (expired protocol window, expired consent form, expired assessment, expired delegation, open deviation, failed assessment result), and the load-bearing standing-after-amendment scenario - admit `proto_v2` after a randomisation under `proto_v1` and confirm the earlier admission survives while a later participant must enrol under `proto_v2`. The PG integration test walks the happy path end to end through `propose_against_pg`, confirming `EvalValue::Date` round-trips through PG JSONB.

---

## Design notes

### What `Expr::DateLe` earned its place for

This example forced `Expr::DateLe` into the kernel, and `Value::Date` / `EvalValue::Date` with it. The validity-window rule

```text
effective_from <= action_date and action_date <= effective_to
```

is the natural shape of a regulated decision: *was this thing in force on this date?* Encoding it without civil-date ordering means either degrading dates to subject literals and comparing them lexicographically (correct for ISO-8601 only as long as nobody mixes formats; brittle to refactoring), or hand-rolling a date predicate outside the kernel (no audit standing, no replay).

`DateLe` is a separate primitive from decimal `Le`. Two semantically distinct ordered domains share no useful generic shape yet; introducing one ahead of a third comparator would be premature. The same discipline applies to date arithmetic, time-of-day values, time zones, durations, and business calendars - all explicitly deferred until a worked example forces them. Civil-date ordering, inclusive `[from, to]` windows, and that alone is the temporal surface of v0.

### Inclusive window semantics

Validity windows are **inclusive on both ends**: `effective_to == randomised_on` admits. This matches how regulatory and clinical language read the windows ("the protocol is valid through 31 March 2026") and is pinned by tests on both endpoints (`boundary_equality_admits_at_protocol_end`, `boundary_equality_admits_at_assessment_expiry`). Half-open `[from, to)` is common in software but not what a non-engineer reading the audit log would assume. The choice is one for the whole runtime, not per-example: once made, it must be invariant across every date-window predicate in every future example.

### What this example deliberately does not cover

The scope of this example is intentionally narrow - just enough to force inclusive civil-date validity-window admission. Real GCP is larger.

- **Adverse-event reporting and SAE timelines.** Real and load-bearing for clinical trials, but governed by reporting-window obligations rather than admission-window gates. A future example that pins SAE reporting would do so against a separate forcing pressure.
- **Investigational product accountability.** Drug receipt, dispensing, return and destruction is its own evidence regime. Out of scope here.
- **eTMF completeness scoring, electronic signatures, audit-trail metadata standards.** Each is its own forcing function. Not blocked on anything here.
- **Protocol deviation lifecycle.** The example admits an `ImportantProtocolDeviationOpen` claim but provides no closure transition. A real deviation has assessment, root cause, CAPA and closure. A worked example for that would pin retraction and supersession patterns the verified-revenue example already covers.
- **Multi-criterion eligibility under `Forall`.** The current example admits if the protocol's *single* matching criterion has a valid assessment. A real trial has many criteria, and the natural shape is a `Forall` over criteria. The kernel already supports `Forall`; the simplification here keeps the example focused on the date-window primitive rather than layering quantification on top. A future example combining multi-criterion eligibility with restatement-on-correction would be the next forcing pressure.
- **Effective-time across an arbitrary number of restatements.** As-of replay handles "what did the system believe at moment T" through `transition_id`. A genuine effective-time worked example - where a protocol amendment is itself restated after publication - would force the runtime to reason about both transaction time and effective time across the same record. That is a future example, not this one.
- **Time of day, time zones, DST, gas day, settlement period, business calendars.** All deliberately deferred. ISO civil dates are sufficient for protocol windows, consent windows, and lab-result expiry; instants, zones and business calendars are required for energy trading, intraday submissions and clearing/settlement workflows, and each will arrive with the worked example that forces them. The kernel design has been chosen to make those additions incremental: `Value::Date` is a civil date with no time-of-day, so a future `Value::Instant` or `Value::Zoned` would be a distinct, separately-named variant, not a retrofit.

### What the GCP framing buys

The regulatory context above is not an aspiration the runtime needs to grow into. It is the existing surface that this example's primitives already cover. `ParticipantRandomised` answers *who admitted whom under what protocol*. `ProtocolVersion`, `ConsentFormVersion` and `DelegatedInvestigator` answer *what rules and authorities were in force* on the date. As-of replay answers *what would the system have admitted at moment T*. A real trial's EDC and IRT systems stay where they are; what Morpholog adds is the evidence kernel that can defend the decisions those systems record - to a sponsor, an auditor, a regulator, or a future investigator looking back across an amendment chain.
