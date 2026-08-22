# Borrowing base

A lender does not advance money against the full value of a borrower's
collateral. It advances a fraction - an *advance rate* - and watches that
the loan never outgrows it. The discipline is simple to state and easy to
breach quietly: as collateral is pledged and money is drawn, the total
drawn must always stay within the advance rate times the eligible
collateral.

This example governs that discipline, and in doing so it completes
Morpholog's decimal arithmetic. Earlier examples only ever added and
subtracted. A proportional rule - *"at most 80% of"* - needs
multiplication, and a reporting ratio - *"what fraction has been drawn?"* -
needs division. So this is the example that forces `Mul` and `Div` into the
kernel.

## The scenario

A lender opens facility `f1` at an **80% advance rate**. The borrower
pledges collateral worth **100**. The lender will now advance up to
`0.8 * 100 = 80`.

A first drawdown of 80 is admitted - exactly the limit. A later drawdown of
40 is refused: the running total would be 120, and `120 > 0.8 * 100`. The
refusal is atomic - no drawdown recorded, no payment intent emitted -
because the candidate state would have breached the advance-limit
invariant. Pledge another 50 of collateral and the base rises to 150; now
the same 40 fits (`120 <= 0.8 * 150 = 120`).

At any point the reporting view answers *how drawn is this facility?* -
`FacilityUtilisation(f1)` is the drawn amount over the pledged collateral,
a rounded fraction for a human to read.

## Exact gates, rounded reports

The two operators sit on the right side of Morpholog's boundary, in
different ways:

- The **advance-limit rule multiplies, and stays exact.** It is written in
  multiplied form - `drawn <= advance_rate * base` rather than
  `drawn / base <= advance_rate` - so the admission decision never depends
  on rounding a quotient. This is `Mul` expressing an admission rule.
- The **utilisation view divides, and may round.** It is a read-side
  derived claim - recomputed on demand, never stored - exactly like the
  ledger's trial balance uses subtraction. A rounded percentage is what a
  report wants. This is `Div` in a projection.

Neither produces a *stored governed figure*: the multiplication is a
threshold inside a rule, the division is a display value recomputed from
the claims it reports on. Business arithmetic that produces governed
figures (a mark-to-market, a price) still lives outside Morpholog and
returns as admitted claims.

## The program

See [`borrowing_base.morph`](borrowing_base.morph) for the surface form.

### Claims

| Predicate | Role |
| --- | --- |
| `Facility(facility, advance_rate)` | The facility and its advance rate (a fraction such as `0.80`), set when it opens. |
| `EligibleCollateral(facility, asset, collateral_value)` | One pledged asset and its value. One pledge per asset (`unique by`), so an asset-keyed question has one answer. |
| `Drawdown(facility, draw_id, amount)` | One advance of money drawn against the facility. |
| `FacilityUtilisation(facility, utilisation)` | A read-only reporting view (the derived claim below), not admitted operational state. |
| `AssetValue(asset, figure)` | The collateral register by asset - a read-only view keyed by the asset alone. |

### Invariants

| Invariant | What it says |
| --- | --- |
| `at_most_one_facility_per_id` | A facility id identifies one facility, so its advance rate is a single well-defined number. |
| `advance_rate_within_unit_interval` | An advance rate is a fraction, `0 <= rate <= 1`. |
| `collateral_value_is_positive` | Every pledged value is strictly positive, so the utilisation divisor is never zero under any admitted state. |
| `drawdown_amount_is_non_negative` | A drawdown moves money out, never in (repayments are out of scope). |
| `outstanding_within_advance_limit` | The total drawn never exceeds `advance_rate * eligible collateral` (in multiplied form, exact). |

### Transformations

| Transformation | Effect |
| --- | --- |
| `open_facility(facility, advance_rate)` | Opens a facility at a fixed advance rate. Refuses a second under the same id. |
| `pledge_collateral(facility, asset, collateral_value)` | Adds an eligible asset, raising the base the advance limit is measured against. |
| `draw(facility, draw_id, amount)` | Records a drawdown and emits a payment-request intent. Operational gates only; the advance limit is the invariant's job. |

### Derived claims

| Derived | Definition |
| --- | --- |
| `FacilityUtilisation(facility, utilisation)` | `drawn / pledged collateral` per facility, over facilities that have collateral - every value of which is positive by invariant, so the divisor is never zero. The one use of division. |
| `AssetValue(asset, figure)` | The collateral register keyed by the asset alone: the facility each pledge names is projected out of the head, and the figure is read by naming its field (`collateral_value: _`) with the facility left unstated. |

## How to run it

```bash
cargo test -p morpholog-examples --test borrowing_base
```

The tests cover a within-limit draw (inclusive at the limit), an
over-limit draw rejected by the invariant, the cumulative-sum limit across
two draws, the utilisation ratio, and the asset register - one row per
asset across facilities, with a re-pledge at a different value refused.

## What this example deliberately does not cover

- **Concentration caps.** "No single obligor may exceed 10% of the pool" is
  a real borrowing-base covenant, and another natural home for
  multiplication. It is left out because as a hard invariant it is
  unsatisfiable for a single-obligor pool (the first pledge is always 100%
  of the pool), and the realistic model reduces an obligor's *eligible*
  value down to the cap - which needs a `min`, an operator the kernel does
  not yet have. A forcing example for `min` is the natural next step.
- **Repayments and revolving draws.** A real facility nets repayments
  against drawdowns. Here drawdowns only accumulate; modelling repayment is
  more claims, no new arithmetic.
- **Collateral eligibility and aging.** Whether an asset is eligible (not
  past due, not an ineligible class) is a gate on `pledge_collateral`, the
  same shape as authority gates in other examples.
