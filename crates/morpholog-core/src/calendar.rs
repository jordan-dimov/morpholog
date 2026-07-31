//! The calendar-span grammar and value: `P[nY][nM][nD]`, or `PnW` alone.
//!
//! Owned here rather than delegated to jiff's span parser, which accepts
//! a wider language (lowercase, signed, fractional, time units). The
//! surface diagnostic path and the evaluator both route through
//! [`parse_calendar_span`], so the two cannot drift.

use serde::{Deserialize, Serialize};

/// A calendar span: whole months plus whole days, kept apart because a
/// month's length depends on the date it lands on and a day's does not.
/// Years normalise to months and weeks to days at parse time; days
/// never normalise into months (`P1M` and `P30D` are different shifts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CalendarSpan {
    pub months: i32,
    pub days: i32,
}

/// Renders the normalised form (`P3M`, `P45D`, `P3M15D`; `P0D` when
/// empty) for error messages and traces. The IR literal keeps the
/// author's source spelling; this is the runtime value's own face.
impl std::fmt::Display for CalendarSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.months == 0 && self.days == 0 {
            return write!(f, "P0D");
        }
        write!(f, "P")?;
        if self.months != 0 {
            write!(f, "{}M", self.months)?;
        }
        if self.days != 0 {
            write!(f, "{}D", self.days)?;
        }
        Ok(())
    }
}

/// The only parse-time bound is the representation's own: each
/// normalised component must fit an `i32`. A span is not intrinsically
/// out of range - whether a shift leaves the calendar depends on the
/// date it is applied to, and that is the evaluator's
/// `ArithOutOfRange`, not the grammar's business.
const MAX_COMPONENT: i64 = i32::MAX as i64;

/// Parse the calendar-span grammar. Uppercase only, unsigned whole
/// numbers only, at least one component, no time part (even
/// zero-valued), weeks standing alone. The error string is
/// author-facing; callers prefix the literal being rejected.
pub fn parse_calendar_span(s: &str) -> Result<CalendarSpan, String> {
    let Some(body) = s.strip_prefix('P') else {
        return Err("expected ISO 8601 date units, e.g. P3M or P45D".to_string());
    };
    if body.contains('T') || body.contains('t') {
        return Err(
            "a calendar span takes date units only (Y/M/W/D); exact time \
             spans are duration(...)"
                .to_string(),
        );
    }
    let mut months: i64 = 0;
    let mut days: i64 = 0;
    let mut saw_week = false;
    let mut component_count = 0u32;
    // Units must appear in order, each at most once.
    let mut allowed: &[char] = &['Y', 'M', 'W', 'D'];
    let mut rest = body;
    while !rest.is_empty() {
        let digits_len = rest.chars().take_while(char::is_ascii_digit).count();
        if digits_len == 0 {
            return Err("expected ISO 8601 date units, e.g. P3M or P45D".to_string());
        }
        let (digits, after) = rest.split_at(digits_len);
        let Ok(n) = digits.parse::<i64>() else {
            return Err("calendar span component out of range".to_string());
        };
        let mut after_chars = after.chars();
        let Some(unit) = after_chars.next() else {
            return Err("expected ISO 8601 date units, e.g. P3M or P45D".to_string());
        };
        let Some(pos) = allowed.iter().position(|&u| u == unit) else {
            return Err(if unit.is_ascii_lowercase() {
                "calendar span units are uppercase (P3M, not P3m)".to_string()
            } else {
                "expected ISO 8601 date units, e.g. P3M or P45D".to_string()
            });
        };
        allowed = &allowed[pos + 1..];
        match unit {
            'Y' => months = months.saturating_add(n.saturating_mul(12)),
            'M' => months = months.saturating_add(n),
            'W' => {
                saw_week = true;
                days = days.saturating_add(n.saturating_mul(7));
            }
            'D' => days = days.saturating_add(n),
            _ => unreachable!("unit list is closed"),
        }
        component_count += 1;
        rest = after_chars.as_str();
    }
    if component_count == 0 {
        return Err("a calendar span needs at least one component, e.g. P3M".to_string());
    }
    if saw_week && component_count > 1 {
        return Err("weeks stand alone (PnW) and do not combine with other units".to_string());
    }
    if months > MAX_COMPONENT || days > MAX_COMPONENT {
        return Err("calendar span component out of range".to_string());
    }
    #[allow(clippy::cast_possible_truncation)] // bounded by the checks above
    Ok(CalendarSpan {
        months: months as i32,
        days: days as i32,
    })
}

#[cfg(test)]
mod tests {
    use super::{CalendarSpan, parse_calendar_span};

    fn span(months: i32, days: i32) -> CalendarSpan {
        CalendarSpan { months, days }
    }

    #[test]
    fn the_accepted_grammar_normalises_to_months_and_days() {
        for (text, expected) in [
            ("P3M", span(3, 0)),
            ("P45D", span(0, 45)),
            ("P1Y", span(12, 0)),
            ("P1Y6M", span(18, 0)),
            ("P1M15D", span(1, 15)),
            ("P2W", span(0, 14)),
            ("P0D", span(0, 0)),
            ("P03M", span(3, 0)),
            // Huge but representable: whether a shift this size leaves
            // the calendar is decided against the date it is applied
            // to, not by the grammar.
            ("P500000M", span(500_000, 0)),
            ("P10001Y", span(120_012, 0)),
        ] {
            assert_eq!(parse_calendar_span(text).unwrap(), expected, "{text}");
        }
    }

    #[test]
    fn the_refused_forms_each_name_their_problem() {
        for (text, needle) in [
            ("PT6H", "duration("),
            ("P1DT0H", "duration("),
            ("P0DT0S", "duration("),
            ("P3m", "uppercase"),
            ("p3M", "P3M or P45D"),
            ("P", "at least one component"),
            ("P-3M", "P3M or P45D"),
            ("P1.5M", "P3M or P45D"),
            ("P1W2D", "weeks stand alone"),
            ("P1M1W", "weeks stand alone"),
            ("P3M1Y", "P3M or P45D"),
            ("P1M1M", "P3M or P45D"),
            ("P99999999999999999999M", "out of range"),
            ("P3000000000D", "out of range"),
            ("3M", "P3M or P45D"),
            ("", "P3M or P45D"),
        ] {
            let err = parse_calendar_span(text).unwrap_err();
            assert!(err.contains(needle), "{text}: {err}");
        }
    }
}
