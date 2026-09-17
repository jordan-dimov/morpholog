use super::read_line_capped;
use std::io::BufReader;

#[test]
fn a_multibyte_character_split_across_buffer_fills_decodes_whole() {
    // A two-byte reader capacity forces the fill boundary through
    // the middle of the two-byte `é`; per-chunk decoding refused
    // this as invalid UTF-8.
    let mut input = BufReader::with_capacity(2, "h\u{e9}llo\n{\"op\":\"x\"}\n".as_bytes());
    let mut line = String::new();
    let n = read_line_capped(&mut input, &mut line).expect("valid line");
    assert_eq!(line, "h\u{e9}llo\n");
    assert_eq!(n, line.len());
    line.clear();
    read_line_capped(&mut input, &mut line).expect("next line intact");
    assert_eq!(line, "{\"op\":\"x\"}\n");
}

#[test]
fn eof_mid_line_returns_what_arrived() {
    let mut input = BufReader::with_capacity(3, "tail without newline".as_bytes());
    let mut line = String::new();
    let n = read_line_capped(&mut input, &mut line).expect("reads to EOF");
    assert_eq!(line, "tail without newline");
    assert_eq!(n, line.len());
    assert_eq!(read_line_capped(&mut input, &mut line).unwrap(), 0);
}

#[test]
fn genuinely_invalid_utf8_is_still_refused() {
    let mut input = BufReader::new(&b"\xff\xfe\n"[..]);
    let mut line = String::new();
    let err = read_line_capped(&mut input, &mut line).expect_err("invalid bytes");
    assert!(format!("{err:#}").contains("not valid UTF-8"));
}
