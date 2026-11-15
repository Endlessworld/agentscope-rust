//! Wire envelope: queue metadata plus the durable event JSON verbatim.
//!
//! The payload is not a re-modelled event: it is [`PublicEvent`] serialised by
//! `agentscope-durable`, so this crate can never drift from the durable wire contract. The
//! header duplicates the three fields the consumer needs *before* decoding the payload
//! (`eventId` for dedupe, `sequence` for ordering, `cursor` for the resume position);
//! [`Envelope::event`] cross-checks them against the payload so a half-rewritten message is
//! rejected instead of trusted.

use agentscope_durable::PublicEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::EnvelopeError;
use crate::message::Message;
use crate::route::Route;

/// The bytes a [`Message`] carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    /// Stable durable event identity; the dedupe key of at-least-once delivery.
    pub event_id: String,
    /// Durable sequence as a number so ordering is checked without string parsing games.
    pub sequence: u64,
    /// Durable cursor of this event inside its session log.
    pub cursor: String,
    /// The durable event JSON, the same rendering [`PublicEvent`] has on the durable wire.
    pub payload: Value,
}

impl Envelope {
    /// Wrap a committed durable event.
    ///
    /// Fails when the event's `sequence` is not a decimal `u64` (the durable runtime only
    /// produces decimal sequences) or when an identity field is empty.
    pub fn from_event(event: &PublicEvent) -> Result<Self, EnvelopeError> {
        let sequence = event.sequence.parse::<u64>().map_err(|_| {
            EnvelopeError::invalid(format!(
                "event sequence {:?} is not a decimal u64",
                event.sequence
            ))
        })?;
        let envelope = Self {
            event_id: event.event_id.clone(),
            sequence,
            cursor: event.cursor.clone(),
            payload: serde_json::to_value(event)?,
        };
        envelope.validate()?;
        Ok(envelope)
    }

    /// Reject an envelope a consumer could not act on.
    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if self.event_id.trim().is_empty() {
            return Err(EnvelopeError::invalid("eventId must not be empty"));
        }
        if self.cursor.trim().is_empty() {
            return Err(EnvelopeError::invalid("cursor must not be empty"));
        }
        Ok(())
    }

    /// Decode the payload and prove the header still describes it.
    pub fn event(&self) -> Result<PublicEvent, EnvelopeError> {
        self.validate()?;
        let event: PublicEvent = serde_json::from_value(self.payload.clone())?;
        if event.event_id != self.event_id
            || event.cursor != self.cursor
            || event.sequence != self.sequence.to_string()
        {
            return Err(EnvelopeError::invalid(
                "envelope header does not match its payload",
            ));
        }
        Ok(event)
    }

    /// Encode for a message body. Symmetric with [`Envelope::decode`].
    pub fn encode(&self) -> Result<Vec<u8>, EnvelopeError> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }

    /// Decode a message body.
    pub fn decode(body: &[u8]) -> Result<Self, EnvelopeError> {
        let envelope: Self = serde_json::from_slice(body)?;
        envelope.validate()?;
        Ok(envelope)
    }

    /// Build the queue message for `route`.
    pub fn into_message(&self, route: &Route) -> Result<Message, EnvelopeError> {
        Message::seal(route, self)
    }
}

#[cfg(test)]
mod tests {
    use agentscope_durable::{PublicEvent, SessionKey};
    use serde_json::json;

    use super::*;
    use crate::route::route;

    fn event(sequence: u64) -> PublicEvent {
        let session = SessionKey::new("tenant", "session").expect("session key");
        PublicEvent {
            schema_version: "1".to_string(),
            event_id: format!("e-{sequence}"),
            session,
            thread_id: "main".to_string(),
            turn_id: "turn-1".to_string(),
            run_id: None,
            step_id: None,
            sequence: sequence.to_string(),
            cursor: format!("cursor:{sequence}"),
            event_type: "turn/status".to_string(),
            data: json!({ "n": sequence }),
        }
    }

    #[test]
    fn an_envelope_round_trips_through_bytes() {
        let envelope = Envelope::from_event(&event(7)).expect("seal");
        let body = envelope.encode().expect("encode");
        let restored = Envelope::decode(&body).expect("decode");
        assert_eq!(restored, envelope);
        assert_eq!(restored.event().expect("event"), event(7));
        assert_eq!(restored.sequence, 7);
    }

    #[test]
    fn a_non_decimal_sequence_is_rejected() {
        let mut event = event(1);
        event.sequence = "1.5".to_string();
        assert!(matches!(
            Envelope::from_event(&event),
            Err(EnvelopeError::Invalid(_))
        ));
    }

    #[test]
    fn a_header_that_disagrees_with_its_payload_is_rejected() {
        let envelope = Envelope::from_event(&event(3)).expect("seal");
        let mut tampered = envelope.clone();
        tampered.sequence = 4;
        assert!(tampered.event().is_err());

        let mut tampered = envelope.clone();
        tampered.cursor = "cursor:99".to_string();
        assert!(tampered.event().is_err());
    }

    #[test]
    fn an_empty_identity_is_rejected_on_decode() {
        let body = json!({
            "eventId": " ",
            "sequence": 1,
            "cursor": "cursor:1",
            "payload": {}
        });
        let encoded = serde_json::to_vec(&body).expect("encode");
        assert!(matches!(
            Envelope::decode(&encoded),
            Err(EnvelopeError::Invalid(_))
        ));

        let not_json = b"not an envelope";
        assert!(matches!(
            Envelope::decode(not_json),
            Err(EnvelopeError::Json(_))
        ));
    }

    #[test]
    fn sealing_uses_the_route_metadata() {
        let session = SessionKey::new("tenant", "session").expect("session key");
        let envelope = Envelope::from_event(&event(1)).expect("seal");
        let message = envelope.into_message(&route(&session)).expect("message");
        assert_eq!(message.key, session.storage_key());
        assert_eq!(Envelope::decode(&message.body).expect("decode"), envelope);
    }
}
