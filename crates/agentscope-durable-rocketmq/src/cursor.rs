//! Queue cursor: the transport position of a consumer inside one partition.
//!
//! The cursor is *not* the durable cursor. The durable cursor addresses an event inside a
//! session log and is carried by each event ([`agentscope_durable::PublicEvent::cursor`]); the
//! queue cursor addresses the next message to fetch and is what a broker can seek to. A host
//! persists both: the offset to resume reading, the durable cursor to prove which event the
//! resume position corresponds to.

use std::fmt::{self, Display, Formatter};

use crate::error::CursorError;

/// A broker-side position: the offset of the first message not yet handed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Cursor(u64);

impl Cursor {
    /// The beginning of a partition.
    pub const START: Self = Self(0);

    /// Wrap a raw broker offset.
    pub const fn from_offset(offset: u64) -> Self {
        Self(offset)
    }

    /// The raw broker offset.
    pub const fn offset(self) -> u64 {
        self.0
    }

    /// Move past `delivered` messages. Saturates instead of panicking on an impossible count.
    pub fn advance(self, delivered: usize) -> Self {
        Self(self.0.saturating_add(delivered as u64))
    }

    /// Parse a persisted cursor. An empty string means [`Cursor::START`], which is what an
    /// uninitialised host field looks like.
    pub fn parse(text: &str) -> Result<Self, CursorError> {
        if text.is_empty() {
            return Ok(Self::START);
        }
        text.parse::<u64>().map(Self).map_err(|_| CursorError::new(text))
    }
}

impl Display for Cursor {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_round_trips_through_its_text_form() {
        for offset in [0, 1, 42, u64::MAX] {
            let cursor = Cursor::from_offset(offset);
            assert_eq!(Cursor::parse(&cursor.to_string()), Ok(cursor));
        }
    }

    #[test]
    fn an_empty_cursor_starts_at_the_beginning() {
        assert_eq!(Cursor::parse(""), Ok(Cursor::START));
        assert_eq!(Cursor::parse("0"), Ok(Cursor::START));
    }

    #[test]
    fn a_non_numeric_cursor_is_rejected() {
        for text in ["abc", "-1", "1.0", " 1", "0x10"] {
            assert!(Cursor::parse(text).is_err(), "{text:?} must not parse");
        }
    }

    #[test]
    fn advancing_saturates_instead_of_wrapping() {
        assert_eq!(Cursor::from_offset(u64::MAX).advance(1).offset(), u64::MAX);
        assert_eq!(Cursor::START.advance(3).offset(), 3);
    }
}
