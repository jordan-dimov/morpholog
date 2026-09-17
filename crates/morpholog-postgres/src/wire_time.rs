//! The one spelling of an instant on the wire: RFC 3339, UTC, `Z`,
//! seconds followed by the shortest of zero, three, six, or nine
//! fractional digits that represents the instant exactly. Every
//! serialized timestamp and every rendered one goes through here, so
//! the bytes an embedder reads are specified in one place and the
//! in-memory clock type can change without moving them. Parsing
//! accepts more than rendering emits: any RFC 3339 offset, normalised
//! to UTC.

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

    /// The spelling is pinned by vector so a change of clock type has
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
