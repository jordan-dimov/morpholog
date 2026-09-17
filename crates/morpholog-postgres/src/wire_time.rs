//! The one spelling of an operational instant on the wire - the
//! adapter's `DateTime<Utc>` values: when a row committed, was
//! rejected, enqueued, attested, or scored. RFC 3339, UTC, `Z`, seconds
//! followed by the shortest of zero, three, six, or nine fractional
//! digits that represents the instant exactly. Every adapter timestamp
//! field and every adapter-rendered instant goes through here, so
//! those bytes are specified in one place and the clock type behind
//! them can change without moving them. Parsing accepts more than
//! rendering emits: any RFC 3339 offset, normalised to UTC.
//!
//! Domain timestamps - a `Timestamp` value inside a claim - belong to
//! the kernel's own codec and are not this module's concern. The
//! module is public on purpose: the CLI's envelopes carry adapter
//! instants and must spell them the same way, so this is part of the
//! adapter's surface, not incidental plumbing.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serializer};

pub fn render(at: &DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

pub fn parse(text: &str) -> chrono::ParseResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text).map(|at| at.with_timezone(&Utc))
}

pub fn serialize<S: Serializer>(at: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&render(at))
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
    let text = <String as Deserialize>::deserialize(d)?;
    parse(&text).map_err(serde::de::Error::custom)
}

pub mod option {
    use super::{DateTime, Deserialize, Deserializer, Serializer, Utc};

    pub fn serialize<S: Serializer>(at: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match at {
            Some(at) => super::serialize(at, s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        <Option<String> as Deserialize>::deserialize(d)?
            .map(|text| super::parse(&text).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
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
        ] {
            assert_eq!(render(&at(input)), rendered, "for {input}");
        }
    }

    #[test]
    fn parsing_inverts_rendering() {
        let instant = at("2026-06-01T12:00:00.000042Z");
        assert_eq!(parse(&render(&instant)), Ok(instant));
        assert!(
            parse("2026-06-01T12:00:00").is_err(),
            "an offset is required"
        );
    }

    #[test]
    fn an_absent_instant_is_null_on_the_wire() {
        #[derive(serde::Serialize)]
        struct Row {
            #[serde(with = "super::option")]
            at: Option<DateTime<Utc>>,
        }
        assert_eq!(
            serde_json::to_string(&Row { at: None }).unwrap(),
            r#"{"at":null}"#
        );
    }
}
