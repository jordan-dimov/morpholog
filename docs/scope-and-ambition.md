# Morpholog: Scope and Ambition

This document explains what Morpholog is for, where it should grow and where it should stop. It is not a feature list. For the exact rules of the runtime, see [runtime semantics](runtime-semantics.md). For current plans, see the [roadmap](roadmap.md). [Design history](design-history.md) records why past choices were made.

## The problem

Business software is full of rules. An order must not use more stock than exists. A payment needs approval. A trade must stay within its limits.

Most applications enforce these rules in several places. They appear in services, database queries, APIs and batch jobs. Copies drift. Changes are hard to check. A mistake may only become visible after money, stock or authority has moved.

AI can write this code faster. But more code is not the answer if someone still has to check every copy.

**Morpholog's aim is to make business rules explicit, enforce them in one place and remove much of the code that repeats them.**

## How it works

Suppose a warehouse holds ten items. One order takes seven. A second order asks for five.

In a conventional system, every route that allocates stock must apply the right checks. In Morpholog, the second allocation is proposed to the same rulebook. It is refused because only three items remain. The record does not change.

The rulebook is called a **programme**. It describes what the business accepts, allows and forbids. Its main ideas are small:

- **Claims** are statements the system has accepted, such as "this stock item holds ten units". They are not proof of what exists in the real world.
- **Transformations** are named business actions, such as "allocate stock" or "approve payment". They are the only authorised way to change those claims.
- **Invariants** are rules that accepted changes must respect, such as "stock cannot be over-allocated".
- **The record** keeps admitted changes and the rules under which they were made. Refused changes leave the governed state unchanged.

Morpholog checks each proposal against the current record and the applicable rules. If it passes, the change and its audit entry are saved together. If it fails, Morpholog can explain the refusal.

This is more than calling a validation function. The check and the change happen in one database transaction. A competing change cannot quietly invalidate the decision under the runtime's stated guarantees.

There are limits. A check covers the cases a change affects. It does not necessarily repair unrelated problems left by older data. The exact guarantees are in [runtime semantics](runtime-semantics.md) and [the computational model](computational-model.md).

## The ambition: build less software

A rule should not need one copy in the API, another in a batch job and a third in a report.

Morpholog could become the foundation for a substantial business application. The rulebook would define its key records, actions and constraints. Ordinary code would handle the interface, external calculations and connections to other systems.

This can remove work in four places:

- **Writing:** application code calls named business actions instead of rebuilding the same checks.
- **Reading:** typed views and generated clients can expose the model without another hand-maintained definition of it.
- **Changing:** developers can inspect rules and test proposed changes against recorded history.
- **Explaining:** the system can show which rule allowed or refused a decision, and what evidence was available.

Some of this already exists. Morpholog has a runtime, audit record, checks, explanation tools, historical rehearsal, generated Python clients and SQL views. Larger application-building workflows are still an ambition.

We should not judge success by lines written in the Morpholog language. Judge it by **code, checks and decisions that no longer need separate implementations**.

Often the best improvement is to remove a question. If the record already determines a value, derive it. Do not ask a caller or agent to supply a second copy. Ask for genuine choices, observations and evidence. Check those inputs rather than trusting them.

## Why AI makes this more important

An AI agent can build features quickly. It can also repeat the same business mistake across many files.

The useful split is simple:

**Agents propose. An approved rulebook decides.**

An agent may write a screen, generate an adapter or suggest a business action. It should not have to reproduce every business rule in that code. A protected Morpholog service can enforce the approved rules independently.

This only works if the agent cannot bypass the service, change the deployed rulebook or write directly to the protected data. Those are deployment requirements, not automatic properties of using Morpholog.

Changing the rulebook is different from using it. New rules need review and approval by people with the right authority. An agent cannot approve its own rule changes. A fully protected approval-to-deployment process is still an open goal ([#627](https://github.com/jordan-dimov/morpholog/issues/627)).

The larger opportunity is not "let AI write anything safely". It is **make business software cheaper to build and change while keeping its most important decisions independently controlled**.

## Where Morpholog stops

Morpholog decides which changes may enter the business record. It is not meant to do every job in the application.

| Morpholog owns | Other software owns |
|---|---|
| Accepted claims, business actions and rules | User interfaces and ordinary application flow |
| The decision and the record of it | Forecasting, optimisation, OCR and AI reasoning |
| Durable requests for actions after a commit | Sending payments, messages and reports |
| Views and evidence derived from its record | Dashboards, integrations and external data sources |

Outside work can return as a new proposal. For example, a pricing engine calculates a value. Morpholog can then decide whether that value may be recorded or used for a business purpose.

Morpholog does not call outside services while deciding a change. It records an outgoing request and lets a separate worker act after commit. That worker may retry. Outside effects are not guaranteed to happen exactly once.

**One business operation must have one honest owner of its data.** Splitting a stock movement into two separate commits can leave half the operation done. If an existing system cannot give Morpholog a sound transaction boundary, that integration is not ready.

## The rules must also be right

A system can enforce the wrong rules perfectly.

A payment limit might come from an old contract. An approval step might exist because of a risk that no longer applies. Two departments might follow policies that contradict each other.

Morpholog can explain why a decision passed or failed *under the rules*. That is not the same as explaining why the business chose those rules.

Over time, we want every important rule to have a clear answer to four questions:

- **Why does it exist?** What purpose or obligation does it serve?
- **Where did it come from?** A contract, law, policy or business decision?
- **Who approved it?** Who can change it?
- **What does it affect?** Which actions and past decisions depend on it?

This does not call for a new theory of every business intention. Start with sources, owners, reasons and examples. Keep them linked to the rules they explain. Agents may help find conflicts and suggest changes. People remain responsible for deciding what the business should want.

This is a direction for future work, not a claim that Morpholog already judges whether policies are wise or lawful.

## What we can and cannot trust

Several questions must stay separate:

1. **Did this proposal follow the deployed rules?** Morpholog can answer within its defined scope.
2. **Are those the right rules?** The business must decide.
3. **Are the input facts true?** The system needs trustworthy sources and checks.
4. **Was the right rulebook deployed and protected?** Operators must enforce access and approval boundaries.
5. **Did the outside action happen?** Delivery, acknowledgement and reconciliation need their own evidence.

The audit record helps reconstruct and check past decisions. Rehearsal can show how recorded proposals would fare under different rules. Neither proves that all facts were true or that future behaviour will be correct.

Trust comes from these parts working together. No single feature supplies all of it.

## One rulebook for one shared responsibility

A set of business actions that changes the same governed records should be checked by the rules that apply to those records. Splitting those actions across unrelated rulebooks can leave gaps.

A large programme may be organised into smaller source files. That should make teamwork easier, not change the rules. The deployed programme still has one identity and one set of decisions. A `.parts` manifest lists the files in order, and the runtime reads them as one text, so the programme hashes as one file holding them would.

Separate programmes make sense when responsibilities and write ownership really are separate.

## How we will know it works

The real test is a working business system, not a clever example or a shorter file.

We should compare Morpholog with a well-built conventional application. Test real changes and real failures. Count both the work removed and the new work created.

Measure:

- Time to build and change a correct working feature.
- How many versions of a business rule still need maintenance.
- What reviewers must check, and which defects reach production.
- The cost of learning the model, moving existing data and operating the system.
- Transaction safety, performance and retries under real workloads.
- How well a decision can be explained, checked and revisited.

Test new applications and carefully chosen parts of existing ones. AI-assisted changes need independent tests, not just the agent's own assessment.

If integration costs more than it saves, use a smaller boundary or ordinary software. A failed experiment is evidence, not a reason to add more features until it looks successful.

## What we will not build

**Keep the decision engine small.** It must remain finite, deterministic and free of hidden external calls. New language features need a real example that cannot be handled well without them.

Morpholog will not become a general programming language, workflow scheduler, AI model, solver or replacement for PostgreSQL. It will not have a switch that skips business rules. Exceptions must themselves be approved and recorded under explicit rules.

These limits apply to the **decision engine**, not every tool around it. Better editors, generated APIs, clients and deployment tools may be useful. Build them when they remove real work. Do not build them just because they are possible.

Our design rules are simple: keep one source of authority; derive instead of duplicating; make limits visible; preserve the meaning of decisions; and prefer removing work to adding machinery.

## The direction

**Write the business rules once. Make them hold. Build less software around them.**

The rules should remain small enough to understand and trust. The systems they support need not be small.
