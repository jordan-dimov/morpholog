//! The one wire spelling of the adapter's own instants (when a row
//! committed, was rejected, enqueued, attested, or scored): RFC 3339, UTC,
//! `Z`, seconds plus the shortest of zero, three, six, or nine fractional
//! digits that is exact. Every adapter timestamp goes through here, so the
//! bytes are defined in one place, independent of the clock type.
//!
//! Parsing accepts more, but only a fixed shape: `YYYY-MM-DD`, then `T`,
//! `t` or a space, `HH:MM:SS`, optionally one to nine fractional digits,
//! and `Z`, `z` or a `+HH:MM` / `-HH:MM` offset, normalised to UTC. That
//! is RFC 3339 without leap seconds or sub-nanosecond fractions. The shape
//! is checked here, not left to the more lenient clock library, because
//! evidence packs are read through this and the audit tree commits to the
//! resulting instant.
//!
//! Timestamps inside claims use the kernel's codec, not this. Public
//! because the CLI's envelopes must spell adapter instants the same way.

use jiff::Timestamp;
use serde::{Deserialize, Deserializer, Serializer};

/// A string that is not an instant in the wire shape.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "`{0}` is not an RFC 3339 instant such as 2026-06-30T12:00:00Z \
     (seconds required; Z or an offset such as +01:00)"
)]
pub struct ParseError(String);

pub fn render(at: &Timestamp) -> String {
    let nanos = at.subsec_nanosecond().unsigned_abs();
    let digits = if nanos == 0 {
        0
    } else if nanos.is_multiple_of(1_000_000) {
        3
    } else if nanos.is_multiple_of(1_000) {
        6
    } else {
        9
    };
    format!("{at:.digits$}")
}

pub fn parse(text: &str) -> Result<Timestamp, ParseError> {
    if !has_wire_shape(text.as_bytes()) {
        return Err(ParseError(text.to_owned()));
    }
    text.parse().map_err(|_| ParseError(text.to_owned()))
}

fn has_wire_shape(b: &[u8]) -> bool {
    let digits = |at: usize, n: usize| {
        b.get(at..at + n)
            .is_some_and(|run| run.iter().all(u8::is_ascii_digit))
    };
    let one_of = |at: usize, set: &[u8]| b.get(at).is_some_and(|c| set.contains(c));
    let date_and_time = digits(0, 4)
        && one_of(4, b"-")
        && digits(5, 2)
        && one_of(7, b"-")
        && digits(8, 2)
        && one_of(10, b"Tt ")
        && digits(11, 2)
        && one_of(13, b":")
        && digits(14, 2)
        && one_of(16, b":")
        && digits(17, 2)
        && &b[17..19] != b"60";
    if !date_and_time {
        return false;
    }
    let mut at = 19;
    if one_of(at, b".") {
        let fraction = b[at + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if !(1..=9).contains(&fraction) {
            return false;
        }
        at += 1 + fraction;
    }
    let offset = &b[at..];
    offset.eq_ignore_ascii_case(b"z")
        || (offset.len() == 6
            && one_of(at, b"+-")
            && digits(at + 1, 2)
            && one_of(at + 3, b":")
            && digits(at + 4, 2))
}

pub fn serialize<S: Serializer>(at: &Timestamp, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&render(at))
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Timestamp, D::Error> {
    let text = <String as Deserialize>::deserialize(d)?;
    parse(&text).map_err(serde::de::Error::custom)
}

pub mod option {
    use super::{Deserialize, Deserializer, Serializer, Timestamp};

    pub fn serialize<S: Serializer>(at: &Option<Timestamp>, s: S) -> Result<S::Ok, S::Error> {
        match at {
            Some(at) => super::serialize(at, s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Timestamp>, D::Error> {
        <Option<String> as Deserialize>::deserialize(d)?
            .map(|text| super::parse(&text).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        parse(text).unwrap()
    }

    /// The spelling is pinned by vectors so a change of clock type has
    /// to reproduce it byte for byte.
    #[test]
    fn renders_the_shortest_exact_fraction_in_utc() {
        for (input, rendered) in [
            ("2026-06-01T12:00:00Z", "2026-06-01T12:00:00Z"),
            ("2026-06-01T12:00:00.123000Z", "2026-06-01T12:00:00.123Z"),
            ("2026-06-01T12:00:00.123456Z", "2026-06-01T12:00:00.123456Z"),
            (
                "2026-06-01T12:00:00.123456789Z",
                "2026-06-01T12:00:00.123456789Z",
            ),
            ("2026-06-01T13:00:00+01:00", "2026-06-01T12:00:00Z"),
            ("2026-06-01T12:00:00.12Z", "2026-06-01T12:00:00.120Z"),
            (
                "2026-06-01T12:00:00.000000001Z",
                "2026-06-01T12:00:00.000000001Z",
            ),
            ("1969-12-31T23:59:59.5Z", "1969-12-31T23:59:59.500Z"),
        ] {
            assert_eq!(render(&at(input)), rendered, "for {input}");
        }
    }

    #[test]
    fn parsing_inverts_rendering() {
        let instant = at("2026-06-01T12:00:00.000042Z");
        assert_eq!(parse(&render(&instant)), Ok(instant));
    }

    /// The accepted language is this module's, not the clock library's:
    /// the refusals include forms the library itself would accept.
    #[test]
    fn accepts_the_rfc_3339_shape_and_nothing_wider() {
        for accepted in [
            "2026-06-01T12:00:00Z",
            "2026-06-01t12:00:00z",
            "2026-06-01 12:00:00Z",
            "2026-06-01T12:00:00.1Z",
            "2026-06-01T12:00:00.123456789Z",
            "2026-06-01T13:00:00+01:00",
            "2026-06-01T12:00:00-00:00",
            "2026-06-01T07:30:00-04:30",
        ] {
            assert!(parse(accepted).is_ok(), "should accept {accepted}");
        }
        for refused in [
            "2026-06-01T12:00:00",
            "2026-06-01",
            "2026-06-01T12:00Z",
            "20260601T120000Z",
            "2026-06-01T12:00:00+0100",
            "2026-06-01T12:00:00-04",
            "2026-06-01T12:00:00Z[UTC]",
            "2026-06-01T12:00:00+01:00[Europe/London]",
            "2026-06-01T12:00:00.Z",
            "2026-06-01T12:00:00.1234567891Z",
            "2026-06-01T23:59:60Z",
            "2026-13-01T12:00:00Z",
            " 2026-06-01T12:00:00Z",
            "2026-06-01T12:00:00Z ",
        ] {
            assert!(parse(refused).is_err(), "should refuse {refused}");
        }
    }

    #[test]
    fn an_absent_instant_is_null_on_the_wire() {
        #[derive(serde::Serialize)]
        struct Row {
            #[serde(with = "super::option")]
            at: Option<Timestamp>,
        }
        assert_eq!(
            serde_json::to_string(&Row { at: None }).unwrap(),
            r#"{"at":null}"#
        );
    }
}
