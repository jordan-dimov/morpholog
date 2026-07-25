# Metered billing: the rounding convention as admission law

A metered supply is billed by multiplying a measured volume by a
tariff rate - and the product almost never lands on a whole penny.
Which payable figure the customer owes is a *convention*, and in most
billing systems that convention lives as a habit inside the engine's
code, where a refactor can silently change it. Here the convention is
a law of the ledger: every line's net is the recomputed
rate-times-volume rounded to the nearest penny (exact halves away
from zero), VAT is the rounded recompute from a declared rate, and a
sealed invoice's total is the sum of its rounded lines. A figure that
disagrees by a penny cannot be committed by anyone.

This is the worked example that forced `round(x, quantum)` into the
kernel: one rounding mode, measured against a real billing
convention, replacing a sign-branched shift-and-remainder define that
repeated its product expression across every branch.

## The programme at a glance

| Claim | Meaning |
|---|---|
| `VatRate(rate_id, rate)` | A tax rate declared as law, under its own identity. |
| `ChargeLine(line, invoice, rate_p_per_kwh, volume_kwh, net_gbp, vat_rate_id, vat_gbp)` | One priced line: the engine's computed figures, admitted only if the rules agree. |
| `InvoiceSealed(invoice, total_gbp)` | The invoice closed at its official total. |

| Rule | What it refuses |
|---|---|
| `line_net_is_the_rounded_recompute` | Any net that is not `round((rate * volume) / 100, 0.01)` - a 1p tamper, a stale figure, a different convention. |
| `line_vat_is_the_rounded_recompute` | Any VAT that is not the rounded recompute from the declared rate the line names. |
| `every_line_names_a_declared_rate` | A line naming a rate nobody declared - the totality companion that keeps the VAT rule from passing emptily. |
| `vat_rate_is_a_fraction`, `charge_inputs_are_non_negative` | Rates outside [0, 1] and negative tariffs or volumes - the accidental credit note. Credits are a different model (copy-negation from a committed line), and the boundary is enforced, not just stated in prose. |
| `sealed_total_is_the_sum_of_its_lines` | A total computed any way other than per-line-then-sum - including the rival round-the-aggregate figure, which genuinely differs (two 0.4p lines: 0.00 per-line, 0.01 aggregate). |

Transformations: `declare_vat_rate`, `add_charge_line`, `seal_invoice`.

## Run it

```bash
morpholog check -v examples/15_metered_billing/metered_billing.morph
morpholog propose examples/15_metered_billing/metered_billing.morph declare_vat_rate \
  --actor tax_admin --args-named '{"rate_id":"vat_reduced","rate":"0.05"}'
```

The `.morph` teaches the domain from scratch - the guided tour lives
there, not here.

## Deliberately not covered

Credit notes and reversals (amounts copy-negate the committed line,
never re-round), corrections after sealing (supersession, as in the
verified-revenue example), and period assignment for volumes. One
convention made law is the point.
