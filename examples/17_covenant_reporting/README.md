# Covenant reporting: the loan's calendar as admission law

A loan agreement builds a reporting calendar: covenant test periods
rolling at three-month intervals, a compliance certificate due within
45 days of each period end, and default consequences priced by the
day once the deadline passes. In most lending systems that calendar
lives in a spreadsheet someone maintains - the deadline is a column,
the lateness is a figure someone typed. Here the calendar is law:
the next test date must be exactly three calendar months after the
prior one (month-end clamping included), the deadline is computed
from the period's own end inside every rule that needs it, and an
overdue notice's day count commits only if it is the record's own
count. A schedule that drifts by a day, a backdated certificate, or
a lateness figure off by one cannot be committed by anyone.

This is the worked example that forced civil-date arithmetic into the
kernel: the `span(P3M)` calendar span shifting a `Date` (months walk
the calendar and clamp at month ends; days step it), and date
subtraction counting the actual days between two dates - the ACT
numerator, so ACT/360-style fractions are plain division. Date math
lives in gates and invariants only; every stored figure is
caller-stated and refused unless exact.

## The programme at a glance

| Claim | Meaning |
|---|---|
| `Facility(facility, first_period_ends)` | The loan, opened with the anchor date its calendar grows from. |
| `TestPeriod(facility, period, ends_on)` | One covenant test period; append-only history. |
| `Follows(next, prior)` | The schedule's chain - no forks, no merges, never re-linked. |
| `Certificate(facility, period, delivered_on)` | A delivered compliance certificate; delivery does not unhappen. |
| `Timely(period)` | Standing: the record accepts the certificate as inside the window. |
| `Overdue(period, as_of, days_late)` | A notice that nothing had arrived as of a date, with the exact day count. |

| Rule | What it refuses |
|---|---|
| `periods_follow_three_month_anniversaries` | Any next period not exactly `prior + span(P3M)` - including the date the calendar's own clamping forbids (30 Nov + three months is 28 Feb, and two hops drift to 28 May where one six-month shift keeps the 30th; the .morph teaches why). |
| `follows_links_periods_of_one_facility` | A schedule link across facilities, or to periods the record does not hold. |
| `timely_certificates_landed_inside_the_window` (with its totality companion) | Timely standing for a certificate past day 45 - or standing with no certificate at all. |
| `overdue_notices_state_the_records_own_lateness` | A notice whose `days_late` differs by even one day from `as_of - deadline`. |
| `overdue_notices_follow_the_deadline`, `overdue_notices_precede_any_delivery` | A notice before the deadline has passed, and - in either proposal order - a notice over a delivered certificate or a certificate backdated under a standing notice. |

Transformations: `open_facility`, `schedule_next_period`,
`submit_certificate`, `accept_timely`, `declare_overdue`.

## Run it

```bash
morpholog check -v examples/17_covenant_reporting/covenant_reporting.morph
morpholog propose examples/17_covenant_reporting/covenant_reporting.morph open_facility \
  --actor agent_bank --args-named '{"facility":"fac_2026_rcf","first_period":"fp_1","first_period_ends":"2026-11-30"}'
```

The `.morph` teaches the domain from scratch - the guided tour lives
there, not here.

## Deliberately not covered

The covenant test itself (a breached ratio is admissible evidence,
not an inadmissible document - governing it is a standing question,
not an admission question), business-day conventions and holiday
calendars (data that changes over time, so they enter as claims with
a named authority when an example forces them), fixed quarter-end
schedules (state the dates; the same rule pins them), and correcting
a wrongly issued notice (supersession, as in the verified-revenue
example).
