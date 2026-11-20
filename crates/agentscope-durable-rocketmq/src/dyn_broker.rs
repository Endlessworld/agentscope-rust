//! Dynamic dispatch over the broker port.
//!
//! [`Broker`] is an `async_trait` port, so `Arc<dyn Broker>` is already usable; [`DynBroker`]
//! exists to keep a concrete broker's topic/tag next to the trait object while implementing
//! `Broker` itself, which is what generic executors and workers require.

use std::sync::Arc;

use async_trait::async_trait;

use crate::broker::Broker;
use crate::error::BrokerError;
use crate::message::Message;

/// A broker behind a trait object; cheap to clone and usable as a `Broker` itself.
#[derive(Clone)]
pub struct DynBroker {
    inner: Arc<dyn Broker>,
}

impl DynBroker {
    pub fn new<B: Broker + 'static>(broker: B) -> Self {
        Self {
            inner: Arc::new(broker),
        }
    }
}

#[async_trait]
impl Broker for DynBroker {
    fn topic(&self) -> &str {
        self.inner.topic()
    }

    fn tag(&self) -> &str {
        self.inner.tag()
    }

    async fn publish(&self, message: Message) -> Result<(), BrokerError> {
        self.inner.publish(message).await
    }

    async fn fetch(&self, cursor: &str, max: usize) -> Result<Vec<Message>, BrokerError> {
        self.inner.fetch(cursor, max).await
    }
}
