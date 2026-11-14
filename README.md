# Morpholog

**Write business rules once. Make them hold.**

Business software is full of rules. Who may approve a payment? How much stock can be allocated? When can a trade settle? What happens when a figure is corrected?

Today those rules are spread across application code, database queries, validation layers and background jobs. Each is written several times. The copies drift apart. And when something goes wrong, finding out why a decision was made is detective work.

Morpholog is an open-source language and runtime for governing business records and decisions, built on Rust and PostgreSQL. You describe what the business allows, requires and forbids. People, applications and AI agents propose actions. Morpholog checks each one against the rules and the current record. If it is allowed, the change is saved in one transaction and recorded. If not, nothing changes, and Morpholog says which rule refused it and why.

The aim is less business logic scattered through your software, and stronger guarantees about the decisions it makes.

## A small example

Here is part of a double-entry ledger:

```morph
predicate JournalEntry(entry_id: Subject, posting_date: Subject, period: Subject)
predicate JournalLine(entry_id: Subject, account: Subject, debit_amount: Decimal, credit_amount: Decimal)
predicate PeriodClosed(period: Subject)

invariant balanced_posted_entry:
    let debits = (sum(d | JournalLine(entry, _, d, _)))
    let credits = (sum(c | JournalLine(entry, _, _, c)))
    JournalEntry(entry, _, _) implies debits = credits

transformation post_simple_entry(
    entry_id, posting_date, period,
    debit_account, credit_account, amount
):
    require entry_id_unused: not JournalEntry(entry_id, _, _)
    require period_is_open: not PeriodClosed(period)
    admit JournalEntry(entry_id, posting_date, period)
    admit JournalLine(entry_id, debit_account, amount, 0)
    admit JournalLine(entry_id, credit_account, 0, amount)
    emit JournalEntryPosted(entry_id)
```

Read it line by line:

- Each `predicate` declares a kind of record: an entry, a line of an entry, a closed period.
- The `invariant` is a rule about the records: for every entry, the debits add up to the credits. Every change is checked against it.
- The `transformation` is an action, and the only way records change. It `require`s that the entry is new and its period still open, `admit`s the entry and its two lines, and `emit`s a notification for other systems.

Those are Morpholog's two building blocks: rules and actions. Everything else is built from them.

An entry that does not balance is refused, and the database stays as it was. The refusal names the rule, the entry and the figures that disagreed:

```json
{
  "status": "rejected",
  "rule": "balanced_posted_entry",
  "witness": [{ "var": "entry", "value": { "type": "subject", "value": "entry_002" } }],
  "compared": { "op": "=", "left": { "type": "decimal", "value": "100" }, "right": { "type": "decimal", "value": "90" } }
}
```

Morpholog calls a record a **claim**: something stated under someone's authority at a moment, not a neutral fact. So a correction is a new claim that replaces the old one, not an edit that erases it.

The [whole ledger](examples/03_double_entry_ledger/) also closes periods, restates them, and rebuilds a trial balance at any past moment.

## Where it fits

Morpholog is not a general-purpose language, and you do not write your application in it. Your application stays in Python, Rust or whatever it is written in, and proposes actions. Morpholog owns the decisions it must be able to defend.

```text
   person, application, optimiser, AI agent
                     |
                     |  proposes an action
                     v
             +----------------+
             |   Morpholog    |   rules and the record
             +----------------+
                |            |
       accepted |            |  refused
                v            v
         the record,      the rule that failed and
       and notifications  the values that failed it;
                |         nothing changes
                v
      ETRM, ERP, ledger, workflow
```

It runs beside the systems you have. Its tables can sit in your existing PostgreSQL database, in their own schema. It differs from what is already in your stack:

- A database `CHECK` constraint sees one row. A Morpholog rule can span many records, and its refusal says which case broke it.
- A policy engine such as OPA or Cedar answers a question about the inputs it is given. Morpholog decides over the record itself, as part of the change, and keeps the history that results.
- A workflow engine orders steps. Morpholog decides whether each step that changes the record is allowed.

You reach it through the command line, a typed Python client generated from your rules, Rust libraries, and SQL views. [How embedding works](docs/embedder-integration.md).

## Why it matters

**Less code, and one place for the rules.** A rule should not need one copy in an API, another in a validation layer, a third in a report and a fourth in a query. A Morpholog programme holds the rules, the actions and the figures they compute. From it, Morpholog generates SQL views of derived figures and of each kind of record's history, a description of those views, and a typed Python client.

**A broken state is refused, not repaired later.** A trade past its limit, a payment without authority, a shipment that allocates more stock than exists: each is refused inside the transaction that would have saved it. An optimiser or an AI agent can propose whatever it likes. Through Morpholog, it cannot save what the rules forbid, and there is no bypass flag.

**The past stays explainable.** Prices are corrected, authority is withdrawn, contracts are amended. A decision made yesterday still has to make sense in terms of what was known yesterday. Morpholog records each change, who made it and the rules it passed. You can rebuild the record at any past moment, and export a tamper-evident history that someone else can check.

**Change the rules without guessing.** Before you tighten a limit, `rehearse` re-decides every past proposal under the new rules and reports each one decided differently, with the rule that refused it. It does not predict the future. It gives you evidence from decisions that actually happened.

## See it in real applications

Each example runs end to end against PostgreSQL; nothing is mocked.

| Example | What Morpholog takes care of |
|---|---|
| [Stock allocation](examples/25_stock_allocation/) | One `ship` action visits every allocation on a shipment, splits an item or ships it whole, keeps stock conserved, and follows the product tree. |
| [Trade lifecycle](examples/10_trade_lifecycle/) | Desk authority, a net position limit, settlement against the official price in force, and price corrections that leave earlier settlements standing. |
| [Margin call run](examples/14_margin_call_run/) | A risk engine's day of calls is refused if it misses an account, includes one it should not, or gets an amount wrong. |
| [Verified revenue](examples/02_verified_revenue/) | A figure is approved for a use, relied on, then corrected, and every decision along the way stays defensible. |

[All the examples](examples/README.md), by business problem, by domain, and by what you want to write.

## Get started

Download the binary for Linux (x86_64 or arm64) or macOS (Apple Silicon) from the [releases page](https://github.com/jordan-dimov/morpholog/releases), or [build from source](docs/install.md). Then:

```bash
git clone https://github.com/jordan-dimov/morpholog.git
cd morpholog
morpholog check examples/03_double_entry_ledger/ledger.morph
```

`check` validates a programme without a database and points at the exact line of any problem. To save changes, read the record as it stood, and see refusals, follow [the developer introduction](docs/developer-intro.md). It builds a small governed ledger end to end, for someone who knows Python and SQL. You will need PostgreSQL 18 or later.

## What it is, and what it is not

Morpholog is under active development. It does not replace PostgreSQL, a user interface, a workflow engine or an AI model.

Its guarantees hold for changes made through it. Ordinary database permissions let only Morpholog's role write its tables. A superuser can still edit them directly, as with any database. `audit verify` catches an edit that leaves the records and the history disagreeing, and one that rewrites both, if you shared a fingerprint of the history beforehand.

Speed depends on the rules, the data and the indexes. A saved change takes about 9ms at worked-example scale. Where rules compile to SQL and their indexes are in place, a change checks only the records it touches, and stays under 10ms from 1,000 to 100,000 records. Other rules are checked in memory and grow with the data, to about 1.5s per change at 100,000 records.

[Common questions](docs/developer-intro.md#questions-you-are-probably-asking) · [Documentation](docs/README.md) · [How it works](docs/computational-model.md) · [Scope and ambition](docs/scope-and-ambition.md) · [Roadmap](docs/roadmap.md)

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
