//! Byte-exact head-and-tail capture of one process stream.
//!
//! A sandbox can produce megabytes of output and a durable turn is a *record*, so only a bounded
//! amount may enter it. Cutting the tail would drop the part of a build log that says why it
//! failed, and cutting the head would drop the command that was run, so the budget is split: half
//! from the head, the rest from the tail, and the payload says how much was dropped.
//!
//! Everything here counts bytes. A character count would under-keep multi-byte text and, worse,
//! would make the recorded size depend on the alphabet the command happened to print.

use std::borrow::Cow;

/// The bytes that survived, how many there were before, and whether anything was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// The kept bytes: `min(total, max)` of them, head first, then tail.
    pub bytes: Vec<u8>,
    /// Byte length before truncation.
    pub total: usize,
    pub truncated: bool,
}

impl Captured {
    /// A lossy view for the JSON payload; [`Captured::bytes`] stays authoritative.
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }
}

/// Keeps at most `max` bytes, from both ends when the stream is longer than that.
///
/// The head takes `ceil(max / 2)` bytes and the tail takes the remainder, so the result is
/// exactly `max` bytes whenever the input is longer. `max == 0` keeps nothing.
pub fn capture(bytes: &[u8], max: usize) -> Captured {
    let total = bytes.len();
    if total <= max {
        return Captured {
            bytes: bytes.to_vec(),
            total,
            truncated: false,
        };
    }
    let head = max.div_ceil(2);
    let tail = max - head;
    let mut kept = Vec::with_capacity(max);
    kept.extend_from_slice(&bytes[..head]);
    kept.extend_from_slice(&bytes[total - tail..]);
    Captured {
        bytes: kept,
        total,
        truncated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_at_the_limit_is_kept_whole() {
        let captured = capture(b"abcd", 4);
        assert_eq!(captured.bytes, b"abcd");
        assert_eq!(captured.total, 4);
        assert!(!captured.truncated);
    }

    #[test]
    fn one_byte_over_the_limit_keeps_both_ends() {
        let captured = capture(b"0123456789", 8);
        assert_eq!(captured.bytes, b"01236789");
        assert_eq!(captured.bytes.len(), 8);
        assert_eq!(captured.total, 10);
        assert!(captured.truncated);
    }

    #[test]
    fn a_zero_budget_keeps_nothing_but_still_counts() {
        let captured = capture(b"abc", 0);
        assert!(captured.bytes.is_empty());
        assert_eq!(captured.total, 3);
        assert!(captured.truncated);
        assert!(!capture(b"", 0).truncated);
    }

    #[test]
    fn a_single_byte_budget_keeps_the_head() {
        let captured = capture(b"abc", 1);
        assert_eq!(captured.bytes, b"a");
        assert_eq!(captured.bytes.len(), 1);
    }

    #[test]
    fn the_budget_counts_bytes_not_characters() {
        // 3 + 3 bytes for two characters.
        let captured = capture("中文".as_bytes(), 5);
        assert_eq!(captured.bytes.len(), 5);
        assert_eq!(captured.total, 6);
        assert!(captured.truncated);
        // The head keeps the whole first character, the tail the last byte of the second.
        assert_eq!(&captured.bytes[..3], "中".as_bytes());
    }
}
