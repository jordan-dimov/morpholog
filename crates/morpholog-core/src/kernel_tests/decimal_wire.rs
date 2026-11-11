//! A decimal crosses the wire as written or not at all. The reader holds
//! a text to the schema pattern and to what the kernel holds exactly, so
//! no route rounds a supplied value; and every decimal the kernel can
//! hold reads back from its own text, so what it wrote it can read.

use crate::EvalValue;
use crate::schema::decimal_from_wire;
use rust_decimal::Decimal;

#[test]
fn a_text_past_what_the_kernel_holds_is_refused_not_rounded() {
    for text in [
        "0.00000000000000000000000000001",
        "1.000000000000000000000000000050",
        "1.00000000000000000000000000000",
        "79228162514264337593543950336",
        "79228162514264337593543950335.0",
    ] {
        let err = decimal_from_wire(text).expect_err(text);
        assert!(err.contains("28 fractional digits"), "{text}: {err}");
    }
    for text in ["1e10", "+1", "01", "1.", "1\u{662}", "1\n"] {
        let err = decimal_from_wire(text).expect_err(text);
        assert!(err.contains("schema pattern"), "{text}: {err}");
    }
}

#[test]
fn a_held_decimal_reads_back_from_its_own_text_with_its_scale() {
    for held in [
        Decimal::MAX,
        Decimal::MIN,
        Decimal::ZERO,
        Decimal::from_str_exact("-0.0").unwrap(),
        Decimal::from_str_exact("0.0000000000000000000000000001").unwrap(),
        Decimal::from_str_exact("1.0000000000000000000000000000").unwrap(),
        Decimal::from_str_exact("100.50").unwrap(),
    ] {
        let text = held.to_string();
        let read = decimal_from_wire(&text).expect(&text);
        assert_eq!(read, held, "{text}");
        assert_eq!(read.scale(), held.scale(), "{text} keeps its scale");
    }
}

#[test]
fn every_json_route_reads_a_decimal_through_the_wire_reader() {
    let too_fine = "0.00000000000000000000000000001";
    for json in [
        format!(r#"{{"type":"decimal","value":"{too_fine}"}}"#),
        format!(r#"{{"type":"quantity","value":{{"amount":"{too_fine}","unit":"USD"}}}}"#),
        format!(r#"{{"type":"collection","value":[{{"type":"decimal","value":"{too_fine}"}}]}}"#),
        r#"{"type":"decimal","value":"1e10"}"#.to_string(),
    ] {
        let err = serde_json::from_str::<EvalValue>(&json).expect_err(&json);
        assert!(
            err.to_string().contains("28 fractional digits")
                || err.to_string().contains("schema pattern"),
            "{json}: {err}"
        );
    }
    let held: EvalValue = serde_json::from_str(r#"{"type":"decimal","value":"100.50"}"#).unwrap();
    assert_eq!(
        serde_json::to_string(&held).unwrap(),
        r#"{"type":"decimal","value":"100.50"}"#
    );
}
