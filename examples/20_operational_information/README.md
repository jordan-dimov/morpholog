# Operational information: what two sources are worth only together

A hidden outcome, a menu of actions, a known loss for every pairing -
and sources whose worth is what they save you: the drop from the best
expected loss achievable blind to the best achievable with their
observation in hand. The worked data is parity: a secret bit that is
the exclusive-or of two others. Each source alone is worthless (the
secret stays fifty-fifty whatever it shows); together they are
decisive. Any accounting that values sources one at a time and adds
the figures gets this case exactly wrong - the information exists
only in the combination, and whether a real portfolio behaves this
way is a question of record.

The division of labour is the point. Searching for good actions is
optimisation, and it stays outside: an untrusted engine files a
CERTIFICATE - its action at every observation with that choice's
exact contribution to expected loss, per-coalition totals, and each
coalition's decision value. The record recomputes every figure from
the admitted cases and refuses a certificate wrong by any amount, or
an action any declared alternative strictly beats. The engine
proposes; the record cross-examines.

This is the worked example that forced the expression-valued `sum`
target into the kernel: expected loss is `sum(probability * loss |
...)`, the sum's own recompute over the cases behind an observation.
The workaround - admitting pre-multiplied weighted-loss claims - is
storing a figure the record can compute, the parameter the record
should make unconstructible.

## The programme at a glance

Construction claims (`Experiment`, `Coalition`, `Action`, `Case`,
`ObservedAs`, `Loss`, `CoalitionPair`) assemble a finite game;
`Sealed` freezes it; certificate claims (`BayesChoice`, `BayesRisk`,
`DecisionValue`) price it.

| Rule | What it refuses |
|---|---|
| `sealed_experiment_has_unit_probability`, `sealed_coalitions_observe_every_case`, `sealed_losses_price_every_outcome`, `sealed_experiment_has_an_action` | Sealing an incomplete game: probabilities that do not sum to exactly one, a case some coalition never observed, an un-priced outcome, an empty action menu. |
| `baseline_observes_nothing` | A sighted baseline - every decision value is measured against acting blind, so a baseline that could distinguish cases would quietly redefine "value" everywhere. |
| `joint_does_not_invent_information`, `joint_preserves_member_information` | A nominated join that is not the join: distinguishing cases neither member can tell apart (an oracle), or lumping cases a member separates (forgetting). |
| `choice_risk_is_the_exact_recompute` | A certified contribution that differs, by any amount, from probability-times-loss summed over exactly the cases behind the observation. |
| `chosen_action_is_bayes_optimal` | A certified action some declared alternative strictly beats. Ties are lawful. |
| `coalition_risk_is_the_sum_of_its_choices`, `decision_value_is_the_risk_given_up` (with its two existence companions) | A total that is not the sum of its parts; a value that is not the baseline's certified risk minus the coalition's own, or one certified before both risks exist. |

Transformations: declare-and-seal construction acts, then
`submit_choice`, `certify_coalition_risk` (refused while any
observation still lacks its choice), `certify_decision_value`. The
read side derives `InformationSummary` and `PairSynergy` - the
parity headline, joint value minus the members' values, read off
certified claims and never stored.

## Run it

```bash
morpholog check -v examples/20_operational_information/operational_information.morph
morpholog propose examples/20_operational_information/operational_information.morph \
  create_experiment --actor harness \
  --args-named '{"experiment_id":"xor_demo","baseline_id":"blind"}'
```

The full XOR experiment - construction, seal, certificates for all
coalitions, and the refusal probes - is driven end to end by
`crates/morpholog-examples/tests/operational_information.rs`. The
`.morph` teaches the domain from scratch - the guided tour lives
there, not here.

## Deliberately not covered

Correcting a certificate after admission (supersession, as the
verified-revenue example models it); entropy, logarithms, or any
information measure beyond decision loss; and optimisation itself -
the search stays outside, because a record that searched would have
opinions. It certifies finite expected-loss arithmetic over a
governed case corpus; it proves no general theorem about
information. What it guarantees: no certified figure in this record
can disagree with the cases it was computed from.
