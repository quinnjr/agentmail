//! In-process event fan-out to blocked and watching clients.

use tokio::sync::broadcast;

use crate::protocol::Event;

/// How many events a slow subscriber may fall behind before it skips some.
/// Delivery is best effort: a subscriber that lags must not treat the hub
/// as the record of what happened. Watchers are told and their stream ends;
/// anything that waits on the hub must re-check the store after a lag.
const CAPACITY: usize = 1024;

/// Broadcasts [`Event`]s to every subscriber.
#[derive(Clone, Debug)]
pub struct Hub {
    tx: broadcast::Sender<Event>,
}

impl Default for Hub {
    fn default() -> Self {
        Hub::new()
    }
}

impl Hub {
    pub fn new() -> Hub {
        let (tx, _) = broadcast::channel(CAPACITY);
        Hub { tx }
    }

    /// Send `event` to current subscribers. Having none is fine.
    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// Receive every event published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

/// Whether `event` involves `agent`: mail it sent or received, or its own
/// presence.
pub fn involves(event: &Event, agent: &str) -> bool {
    match event {
        Event::Delivered { to, message } => to == agent || message.from == agent,
        Event::Presence { agent: view } => view.agent.address == agent,
    }
}
