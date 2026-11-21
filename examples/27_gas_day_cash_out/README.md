# Gas Day Cash-out

We charged a shipper for Monday's imbalance on Tuesday. On Thursday a Monday trade nobody had reported turns up, and the price moves. Which charge was right, and what has to change?

## The scenario

GB gas is traded and moved by the gas day, 05:00 to 05:00 London time: 04:00 UTC in summer, and 23 or 25 hours on the days the clocks change. A shipper nominates how much gas it will take out of the network, by 13:00 London on the day before, judged on when the network's own system received the nomination. After the day, each shipper's imbalance is cashed out at a price that starts from the System Average Price (SAP), the quantity-weighted average of the day's balancing trades. A day without trades takes the mean of the seven days before it, so quiet days lean on earlier ones.

The programme decides what may happen: a nomination is admitted only on a receipt that only the network's receiver may record, before a deadline read from a published calendar, by a party allowed to nominate. And it explains what happened later: a late trade makes the price it should have changed stale, and every quiet day that leaned on it, and flags the statement that used it. A correction is a new determination and a new statement beside the original, which still names the price it used and stays lawful on the evidence of its time.

The rules are the Uniform Network Code: TPD Sections C and F v6.27 (14 September 2026) and General Terms Section C v6.28 (9 November 2026). An independent reference, written from that text with exact fractions, computed the figures the tests pin before this programme existed.

## The programme

[`gas_day_cash_out.morph`](gas_day_cash_out.morph) teaches the domain and the reasoning at each construct.

| Claims | |
| --- | --- |
| `LocalDay(calendar, local_date, starts_at, ends_at, tzdb_version, tzdb_digest)` | A row of a published calendar: gas days from 05:00, deadlines at 13:00. |
| `Received(message, boundary, received_at)` | When the network's system acknowledged a message. Only its receiver may record one. |
| `OutputNomination(message, shipper, gas_day, qty_kwh)` | An admitted nomination, first or renomination. |
| `Renomination(message, effective_at)` | A nomination that replaces the one in force, which still applies until the hour the shipper names. |
| `CurrentNomination(shipper, gas_day, message)` | The nomination in force, replaced only through `Renominates`. |
| `ImpliedFlowRate(shipper, gas_day, kwh_per_hour)` | A view: the rate the nomination in force implies from its effective hour to the end of the day, from the chain that led to it. |
| `OcmTrade(trade, gas_day, qty_kwh, price_p_per_kwh)` | A balancing trade, from the appointed market data source. |
| `SapDetermination(version, gas_day, sap_p_per_kwh, precision)` | One SAP, with an identity, the policy it was kept to, and (`SapUsedTrade`, `SapUsedPrior`) the evidence it used. |
| `CurrentSap(gas_day, version)` | The day's current determination, replaced only through `SapSupersedes`. |
| `Imbalance(imbalance, shipper, gas_day, imbalance_kwh)` | A shipper's imbalance for a day, with an identity a statement names. |
| `Statement(statement, shipper, gas_day, sap_version, imbalance, ...)` | A cash-out statement: the SAP version and imbalance it used, both SMPs, the charging policy and price, and one signed cash flow, positive when National Gas pays the shipper. |
| `CurrentStatement(shipper, gas_day, statement)` | The current statement, corrected only through `StatementCorrects`. |
| `NeedsReview(statement)` | A view: current statements whose SAP no longer matches the evidence. |

| Rules | |
| --- | --- |
| `stale(v)` | A determination's named inputs differ from the evidence now held, directly or through any day it leaned on. |
| `received_by_the_network`, `in_time` | A nomination's receipt comes from the network's own system, and is before the published 13:00 deadline on the day before. |
| `first_or_stale` | A new determination or statement replaces the current one only when that one is stale. |
| `previous_in_force`, `after_the_previous` | A renomination names the nomination it revises, which must still be the one in force, and must have been received before it. |
| `in_window`, `within_the_day`, `on_the_hour`, `lead_time`, `in_order`, `rate_not_negative` | A renomination's receipt between 15:00 London the day before and 03:00 on the gas day; an effective hour inside the day, on the hour, at least an hour after the receipt, and not before the one it replaces takes effect; and no change so deep and late that the gas would have to flow backwards. |
| `sap_not_stale`, `inputs_current` | No statement uses, and no quiet day leans on, a SAP the record already knows is stale. |

| Acts | |
| --- | --- |
| `publish_local_day` | A calendar row, as `generate local-days` writes it, from the calendar's publisher. |
| `receive`, `nominate_output`, `renominate` | A receipt from the network's receiver; a nomination or a renomination judged on it. |
| `report_trade`, `report_buy_action`, `report_sell_action`, `report_actions_complete`, `publish_default_smp` | Market evidence, from its source. |
| `determine_sap`, `determine_fallback_sap` | A SAP computed from the record, naming what it used. |
| `record_imbalance`, `issue_statement` | A shipper's imbalance; a statement computed from the current SAP. |

## Policies of the example's own

The code states no precision for SAP, for charging or for a flow rate, and a decimal cannot hold every digit of a chain of quiet-day means, of a long price times a large volume, or of a long quotient added to a rate. So the example names three policies. The statements and determinations record the first two; the third applies to a rate that is computed, never stored:

- `example_sap_precision_v1`: each SAP to eight decimal places, halves away from zero.
- `example_charging_price_4dp_v1`: the calculated SMP to four decimal places, halves away from zero, before it is multiplied by the imbalance. The charge is in pence and is not rounded to whole pence.
- `example_flow_rate_precision_v1`: each term of a nomination's implied flow rate, the first quantity's 1/24 share and each renomination's change over the hours left, to six decimal places of kWh an hour, halves away from zero, before they are added.

They are illustrative, not the code's settlement calculation, so these are not production-accurate cash-out amounts. Two readings of the code are the example's too. On a day National Gas took no balancing action, SMP is SAP plus or minus the default margin, and only once the day's actions are reported complete. And a renomination may not take effect before the one it replaces (`example_renomination_order_v1`): the code says a renomination replaces the nomination it names, which still applies until the new one's effective time (C4.1.5(a), C4.1.8), but not what an earlier effective time does to it.

## How to run it

```
morpholog check examples/27_gas_day_cash_out/gas_day_cash_out.morph -v

# After the set-up acts appoint `calendar_publisher` for the `gas_day` calendar:
morpholog generate local-days examples/27_gas_day_cash_out/gas_day_cash_out.morph publish_local_day \
  --actor calendar_publisher --zone Europe/London --at 05:00 \
  --from 2026-10-12 --until 2026-10-25 --arg calendar=gas_day \
  | morpholog propose examples/27_gas_day_cash_out/gas_day_cash_out.morph --batch - --database-url "$URL"
```

The tests in `crates/morpholog-examples/tests/gas_day_cash_out.rs` walk the story: the nomination admitted, refused as late, refused for authority, and a forged receipt refused; seven trade days and two chained quiet days; the statement; the late trade, the review it raises, and the correction; and renominations on the 25-hour gas day, each refused by the clause it breaks or admitted with the rate the reference computed.

## What it does not cover

Two limits of this slice:

- A statement names its SAP determination and its imbalance, but not which National Gas action, if any, set the SMP. Every action for the day is on the record, and the statement's SMP figures show which limb applied, but the action itself is not named.
- The authorities rest on a trusted start: `appoint_operator` is first come, first served, so a deployment's first act must be its operator's. On a closed deployment each name can be asserted only by the logins granted it, so a shipper's login cannot act as the operator or a receiver, and cannot appoint a receipt authority or record a receipt for itself.

The renominations leave out the 15-minute lead time of very large daily-metered sites (every site here takes the hour), the network's own NDM renominations, the deemed approval of C4.1.7, a "New Renomination" with no earlier nomination to revise, and revising a first nomination before its 13:00 deadline: a second first nomination for the day is refused.

Not modelled yet: trade nominations, the OCM operator's role, Gemini's clock-change outage, completeness evidence for closing a receipt window, the EU timetable, capacity, and multi-day trades. Those are the next steps for this example, on the same programme.
