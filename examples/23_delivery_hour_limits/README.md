# Delivery-hour limits: a cap that holds in every hour

A power book has a cap on its net position, and the cap holds in
every delivery hour, because every hour settles on its own. A trade is
one claim with two instants on it, so the rule has to range over the
hours the trade touches and ask, for each, whether the book still
fits. Summing every trade that overlaps the new one is the tempting
shortcut, and it is wrong: it adds trades that never share an hour.

Here the record builds the grid of hours from the trade itself, so the
caller passes the trade and nothing else, and a rule says how long a
window may be before the grid is built. A day product takes its window
from a published calendar day, 23, 24 or 25 hours long, and the same
hour-by-hour check runs over whatever the day has.

This is the worked example that forced `periods(from, until, step)`:
the start of every period a window touches, computed in the rule.

## The programme at a glance

| Claim | Meaning |
|---|---|
| `Limit(book, cap_mw)` | The book's cap on its net position in any one hour. |
| `Trade(book, trade, mw, starts_at, ends_at)` | A captured trade: signed megawatts over a half-open window of instants. |
| `DeliveryDay(calendar, local_date, starts_at, ends_at, tzdb_version)` | A local day's bounds as a calendar authority published them, with the tzdb version they came from. |
| `CalendarPublisher(calendar, publisher)` | Who may publish a calendar's days. |

| Rule | What it refuses |
|---|---|
| `limits_are_positive` | A cap that is not a positive figure. |
| `a_delivery_day_is_a_civil_day` | A published day that is not exactly 23, 24 or 25 hours long. |
| `capture`'s gates, in order | A window running backwards or longer than a month; a window off the hour grid, since it touches more hours than its length holds; and the trade that would take any hour's net position past the cap, named by the hour. |
| `capture_day`'s gates | The same, with the window read from the published day, so the caller cannot state it and an unpublished day cannot be traded. |

Transformations: `set_limit`, `appoint_calendar_publisher`,
`publish_delivery_day`, `capture`, `capture_day`.

The limit is a gate by design: a pre-trade check at capture. Lowering
a cap under open trades is a breach to manage, another act, not
covered here.

## Run it

```bash
morpholog check -v examples/23_delivery_hour_limits/delivery_hour_limits.morph
morpholog propose examples/23_delivery_hour_limits/delivery_hour_limits.morph set_limit \
  --actor risk --args-named '{"book":"book_a","cap_mw":"10"}'
```

The `.morph` teaches the domain from scratch - the guided tour lives
there, not here.

## Deliberately not covered

Quarter-hour products (the same grid with a 15-minute step), a cap
that changes over time, the breach raised when a cap is lowered under
open trades, and where the published days come from. The authority's
days are trusted as claims; the record governs what is done with them.
