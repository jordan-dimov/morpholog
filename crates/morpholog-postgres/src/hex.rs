//! Lowercase hex encoding, in one place.
//!
//! Four callers wanted it - tree-head signatures, public keys, rendered
//! hashes, and the outbox idempotency key - and each had grown its own
//! per-byte `format!`, which allocates a `String` for every byte. Two of
//! those sites are hot: an idempotency key is computed for every emitted
//! intent, and a rendered hash for every audit row a checkpoint covers.

use std::fmt::Write;

/// Lowercase hex, two characters per byte, into a single allocation.
pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing into a String cannot fail.
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::encode;

    #[test]
    fn pads_each_byte_to_two_digits() {
        // The padding is the whole point: without it, 0x0a would render as
        // "a" and two different byte strings could share one rendering.
        assert_eq!(encode(&[0x00, 0x0a, 0xff, 0x10]), "000aff10");
    }

    #[test]
    fn the_empty_slice_is_the_empty_string() {
        assert_eq!(encode(&[]), "");
    }
}
