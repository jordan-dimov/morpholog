# Documentation

Where to read next, by what you want to know. The words Morpholog uses are
in the [glossary](#glossary) at the end.

## Start here

- [The README](../README.md) - what Morpholog does, and where it sits beside
  the systems you already run.
- [The worked examples](../examples/README.md) - find one by the business
  problem you have, or by what you want to write.
- [A gentle introduction for developers](developer-intro.md) - about half an
  hour from nothing to a programme that refuses a change and explains why.
- [Installing from a release](install.md) - a prebuilt binary to a running
  example, and how an existing database upgrades.

## What Morpholog is, and is not

- [Scope and ambition](scope-and-ambition.md) - what Morpholog is for, and
  what it refuses to become.
- [The computational model](computational-model.md) - why every decision
  finishes and depends only on what it is given, and what that rules out.
- [The outbox](outbox-sketch.md) - how an accepted change reaches other
  systems after it is saved, and why that is retried or compensated, never
  rolled back.

## Building on it

- [Embedding Morpholog](embedder-integration.md) - the pinned contract for
  any language: the request and response shapes, the generated Python
  client, deploying, batches, several proposals as one decision, and
  retries.

## Reference and design

- [Runtime semantics](runtime-semantics.md) - exactly what the rules mean,
  including the full table of what the language can say.
- [Design history](design-history.md) - which worked example forced each
  design decision, and why.
- [Prior art](prior-art.md) - the theory underneath, and what was
  deliberately left out.
- [Roadmap](roadmap.md) - what comes next, what waits for an example, and
  what is out of scope.

## Working on Morpholog

- [Contributing](../CONTRIBUTING.md) - building, testing and the rules for a
  change.
- [Refactoring playbook](refactoring-playbook.md) - making a type change
  that touches dozens of sites, safely.
- [Benchmarking](benchmarking.md) - how performance is measured, and the
  rules that keep the measurement honest.

## Glossary

Morpholog's words for things you already have.

| You might say | Morpholog says | What it means |
|---|---|---|
| a record, a row | **claim** | A statement the record has accepted and stands behind, such as "this trade was captured at this price". Claims are added and retracted, never edited. A claim records what was accepted, not an objective fact. The audit log says which actor was asserted, which database login proposed it, and under which programme and semantics it was accepted. |
| an id, an account, an entity | **subject** | An opaque identifier that claims are about. Morpholog has no types over subjects: what an account is follows from the claims about it. |
| a business rule, a control | **invariant** | A rule no accepted change may break. It is checked whenever a change could affect it, so a change that would break it is refused. |
| an action, a workflow step | **transformation** | A named action with parameters. The only way the record changes. |
| a precondition, an approval check | **gate** (`require`) | A condition on one action, checked when the action is proposed. Withdrawing an authority later stops future actions and leaves past ones standing. |
| a request, a submission | **proposal** | One call of a transformation, with its values and who is acting. It is accepted whole or refused whole, and a refusal names the rule and the values that failed it. |
| a user, a system, an approver | **actor** | Whoever a proposal says is acting, recorded with every change. A name can be restricted so that only an authorised database login may act under it. |
| a notification, a message to another system | **intent** (`emit`) | A message declared in the rules and saved with the change. A separate worker attempts delivery only after the change is saved, retries a failure that may pass, and records one that cannot. |
| a report, a view | **derived claim** (`derived`) | A read-side figure computed from admitted claims. It is never admitted itself, so no rule can depend on it. |
| the ruleset, the policy version | **programme**, **model hash** | The `.morph` file: the claims, rules and actions together. The model hash identifies the whole programme by what it means; comments and layout do not affect it, and every accepted change records it. |
| the audit trail | **audit log**, **checkpoint** | One row per accepted change: the asserted actor and the login behind it, its values, what it added and removed. `audit verify` checks that the log and the records agree. A checkpoint commits to the history so far; held outside the database, it lets someone detect a later rewrite of that history. |
| evidence for an auditor | **evidence pack** | A file written by `audit export` that `audit verify-pack` checks offline, with no database. |
