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
| `TradeNomination(message, shipper, gas_day, side, counterparty, qty_kwh)` | One side of a trade between two shippers: disposing or acquiring, naming the other. |
| `TradeEffective(first, second)`, `TradeRejected(message, reason)`, `TradeWithdrawn(message, withdrawal)` | A pair that took effect; a side rejected, for want of a counterpart in time or for a quantity that differs; a side withdrawn before the day. |
| `SetAside(message, reason)` | A message the network's system read and took as no nomination: malformed, of a kind the example does not model, or refused. |
| `CompleteThrough(boundary, until)` | The network's system says every message it received before `until` is on the record. It only moves forward, and no receipt before `until` may be recorded after it. |
| `Unavailable(system, starts, ends, source)`, `RenominationDeferral(system, starts, ends, processed_at, source)` | A stretch in which the network's system takes no renomination or trade nomination; one in which it holds renominations until `processed_at`. Each names the notice it comes from. |
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
| `corresponding`, `in_time` | Two trade nominations pair only on the same day, on opposite sides, each naming the other's shipper, the second by 06:00 on the day when the first came before it, otherwise within the hour. |
| `record_complete`, `every_receipt_read`, `no_counterpart_in_time`, `record_open` | A side is rejected for want of a counterpart only once the network's record is complete past the deadline, every message received by the deadline has been read, and no waiting counterpart came in time; and no receipt may then be recorded inside the span the record was said to be complete for. |
| `unread` | A message is read once: one already taken as a nomination or set aside is not read again, as an output nomination, a renomination, a trade nomination or a withdrawal. |
| `no_equal_pair`, `unpaired` | A pair is not rejected for unequal quantities while either side has an equal partner waiting in time; a side already paired is not withdrawn. |
| `in_window`, `within_the_day`, `on_the_hour`, `lead_time`, `in_order`, `rate_not_negative` | A renomination's receipt between 15:00 London the day before and 03:00 on the gas day; an effective hour inside the day, on the hour, at least an hour after the receipt, and not before the one it replaces takes effect; and no change so deep and late that the gas would have to flow backwards. |
| `available`, `lead_after_processing`, `nothing_read_in_it` | A renomination or trade nomination received in an outage is refused as unavailable, not as late; a renomination held back counts its hour's lead from when it was processed; and an outage or deferral is announced before any message it governs, received in it, is read: an outage governs renominations and trade nominations, a deferral renominations only. |
| `sap_not_stale`, `inputs_current` | No statement uses, and no quiet day leans on, a SAP the record already knows is stale. |

| Acts | |
| --- | --- |
| `publish_local_day` | A calendar row, as `generate local-days` writes it, from the calendar's publisher. |
| `receive`, `nominate_output`, `renominate` | A receipt from the network's receiver; a nomination or a renomination judged on it. |
| `trade_nominate`, `match_trades`, `expire_trade`, `withdraw_trade`, `set_aside`, `report_complete_through` | One side of a trade, from the shipper's nominator; pairing and expiry, by the network's system; a withdrawal before the day; a message the network takes as no nomination; how far the network's record is complete. |
| `announce_outage`, `announce_deferral` | The network's system says when it will be unavailable, or hold renominations back. |
| `report_trade`, `report_buy_action`, `report_sell_action`, `report_actions_complete`, `publish_default_smp` | Market evidence, from its source. |
| `determine_sap`, `determine_fallback_sap` | A SAP computed from the record, naming what it used. |
| `record_imbalance`, `issue_statement` | A shipper's imbalance; a statement computed from the current SAP. |

## Policies of the example's own

The code states no precision for SAP, for charging or for a flow rate, and a decimal cannot hold every digit of a chain of quiet-day means, of a long price times a large volume, or of a long quotient added to a rate. So the example names three policies. The statements and determinations record the first two; the third applies to a rate that is computed, never stored:

- `example_sap_precision_v1`: each SAP to eight decimal places, halves away from zero.
- `example_charging_price_4dp_v1`: the calculated SMP to four decimal places, halves away from zero, before it is multiplied by the imbalance. The charge is in pence and is not rounded to whole pence.
- `example_flow_rate_precision_v1`: each term of a nomination's implied flow rate, the first quantity's 1/24 share and each renomination's change over the hours left, to six decimal places of kWh an hour, halves away from zero, before they are added.

They are illustrative, not the code's settlement calculation, so these are not production-accurate cash-out amounts. Other readings of the code are the example's too. On a day National Gas took no balancing action, SMP is SAP plus or minus the default margin, and only once the day's actions are reported complete. And a renomination may not take effect before the one it replaces (`example_renomination_order_v1`): the code says a renomination replaces the nomination it names, which still applies until the new one's effective time (C4.1.5(a), C4.1.8), but not what an earlier effective time does to it. And a trade nomination with no counterpart in time is rejected only once the network's system reports its record complete past the counterpart's deadline, so a timely counterpart cannot arrive after the rejection: an evidence rule, not a clause. A complete record of receipts is not yet a complete record of trade nominations, so every message received by the deadline must also have been read, as a nomination of some kind or set aside (`example_all_receipts_read_v1`). A trade nomination names no trade, so with several sides waiting the network chooses the pair, but may not reject one for unequal quantities while either side has an equal partner waiting in time (`example_trade_pairing_v1`). Nothing amends a trade nomination: the code forbids amendment within the day and is silent before it.

The clock change of 25 October 2026 comes from National Gas's notice, "Gemini October 2026 Clock Change Supporting Information", not from the code. Gemini takes no renomination or trade nomination from 01:00 to 01:59:59 GMT, and processes renominations received from 01:55 BST to 02:25 GMT at 02:25 GMT (slides 4 and 7). Slide 3 says those placed in the 01:00 to 02:00 BST hour are approved at 02:25 GMT, a different start. The example follows slide 7, the specific one, as its own reading, not a settlement of the conflict. Under slide 3 the renomination received at 00:30 GMT would be deferred and refused for its lead time, and with it every later renomination of that shipper, each naming it or one after it. The code counts a renomination's lead time from its submission (C4.1.10), but one processed later cannot have given the network that notice, so a deferred renomination's hour counts from its processing (`example_deferred_lead_time_v1`). A deferral of up to 90 minutes also outruns the code's deemed approval an hour after submission (C4.1.7(a)), which the notice does not mention. The example records the conflict and does not resolve it.

## How to run it

```
morpholog check examples/27_gas_day_cash_out/gas_day_cash_out.morph -v

# After the set-up acts appoint `calendar_publisher` for the `gas_day` calendar:
morpholog generate local-days examples/27_gas_day_cash_out/gas_day_cash_out.morph publish_local_day \
  --actor calendar_publisher --zone Europe/London --at 05:00 \
  --from 2026-10-12 --until 2026-10-25 --arg calendar=gas_day \
  | morpholog propose examples/27_gas_day_cash_out/gas_day_cash_out.morph --batch - --database-url "$URL"
```

The tests in `crates/morpholog-examples/tests/gas_day_cash_out.rs` walk the story: the nomination admitted, refused as late, refused for authority, and a forged receipt refused; seven trade days and two chained quiet days; the statement; the late trade, the review it raises, and the correction; renominations on the 25-hour gas day, each refused by the clause it breaks or admitted with the rate the reference computed; trade nominations that pair, are rejected, expire, wait for a message read late, or are withdrawn, as the reference says; and the clock change, with renominations refused in the outage, one held back too long for its hour, others admitted, and announcements refused once something in them was read. The adapter's tests (`crates/morpholog-postgres/src/completeness_race.rs`) race a receipt against the report that the record is complete: a receipt that read the record open commits and is ordered before the report, and one that did not is refused, and an expiry and a timely receipt it did not see never both commit.

## What it does not cover

Two limits of this slice:

- A statement names its SAP determination and its imbalance, but not which National Gas action, if any, set the SMP. Every action for the day is on the record, and the statement's SMP figures show which limb applied, but the action itself is not named.
- The authorities rest on a trusted start: `appoint_operator` is first come, first served, so a deployment's first act must be its operator's. On a closed deployment each name can be asserted only by the logins granted it, so a shipper's login cannot act as the operator or a receiver, and cannot appoint a receipt authority or record a receipt for itself.

The renominations leave out the 15-minute lead time of very large daily-metered sites (every site here takes the hour), the network's own NDM renominations, the deemed approval of C4.1.7, a "New Renomination" with no earlier nomination to revise, and revising a first nomination before its 13:00 deadline: a second first nomination for the day is refused.

Trade nominations leave out their effect on each shipper's imbalance (TPD E5): the example records an imbalance as its owner reports it. Also left out: withdrawing one side of a pair already effective, which the code allows before the day without saying what becomes of the other side; National Gas's system trades, single-sided trade nominations, and the OCM operator's nominations on behalf of both sides.

The clock change leaves out the deferred approval times of OTC trades (slide 9), ICE closing the OCM at 01:50 BST (slide 10), and EU nominations.

Not modelled yet: the OCM operator's role, the EU timetable, capacity, and multi-day trades. Those are the next steps for this example, on the same programme.
