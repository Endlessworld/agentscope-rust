//! Error types of the event channel.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

/// A transport-level failure reported by a [`crate::Broker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerError {
    /// The broker could not accept or serve the request; retrying the same call may succeed.
    Unavailable(String),
    /// The request itself is wrong (foreign topic/tag, cursor past the end); retrying cannot help.
    Invalid(String),
}

impl BrokerError {
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    /// Whether re-issuing the same call can plausibly succeed.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

impl Display for BrokerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) => write!(formatter, "broker unavailable: {message}"),
            Self::Invalid(message) => write!(formatter, "broker rejected the request: {message}"),
        }
    }
}

impl Error for BrokerError {}

/// A malformed or internally inconsistent event envelope.
#[derive(Debug)]
pub enum EnvelopeError {
    /// A header field is empty, a sequence is not a decimal `u64`, or payload and header disagree.
    Invalid(String),
    /// The body is not JSON, or the payload is not a durable public event.
    Json(serde_json::Error),
}

impl EnvelopeError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

impl Display for EnvelopeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "invalid event envelope: {message}"),
            Self::Json(error) => write!(formatter, "invalid event envelope: {error}"),
        }
    }
}

impl Error for EnvelopeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl From<serde_json::Error> for EnvelopeError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// A queue position that is not a decimal offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorError {
    /// The text that failed to parse, kept verbatim for the log line.
    pub text: String,
}

impl CursorError {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl Display for CursorError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid queue cursor {:?}: expected a decimal offset",
            self.text
        )
    }
}

impl Error for CursorError {}

/// A failure while consuming one batch. No variant is safe to ignore.
#[derive(Debug)]
pub enum ConsumeError {
    /// The broker failed; the consumer did not advance.
    Broker(BrokerError),
    /// A message body or payload was unreadable; the consumer did not advance.
    Envelope(EnvelopeError),
    /// The batch is not admissible (wrong topic/tag, foreign session, empty batch request).
    Invalid(String),
    /// A fresh event carried a sequence that does not strictly increase.
    ///
    /// Silently accepting it would corrupt the caller's replay position, so the whole batch is
    /// rejected and the cursor stays where it was.
    OutOfOrder {
        /// Last sequence the consumer accepted.
        previous: u64,
        /// Sequence of the rejected event.
        next: u64,
        /// Identity of the rejected event, for the log line.
        event_id: String,
    },
}

impl ConsumeError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

impl Display for ConsumeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Broker(error) => write!(formatter, "{error}"),
            Self::Envelope(error) => write!(formatter, "{error}"),
            Self::Invalid(message) => write!(formatter, "inadmissible batch: {message}"),
            Self::OutOfOrder {
                previous,
                next,
                event_id,
            } => write!(
                formatter,
                "event {event_id} has sequence {next} at or before the accepted sequence {previous}"
            ),
        }
    }
}

impl Error for ConsumeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Broker(error) => Some(error),
            Self::Envelope(error) => Some(error),
            Self::Invalid(_) | Self::OutOfOrder { .. } => None,
        }
    }
}

impl From<BrokerError> for ConsumeError {
    fn from(error: BrokerError) -> Self {
        Self::Broker(error)
    }
}

impl From<EnvelopeError> for ConsumeError {
    fn from(error: EnvelopeError) -> Self {
        Self::Envelope(error)
    }
}
