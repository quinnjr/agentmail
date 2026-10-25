//! In-process event fan-out to blocked and watching clients.

use std::collections::HashSet;

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

/// What a blocked request (`ask`, `await_answer`, `wait_for_mail`) is
/// waiting for. The hub only wakes the waiter; whether the wait is over is
/// decided from the store (see `Handler::check`), so an event dropped by a
/// lagging subscriber is never mistaken for nothing having happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wait {
    /// An answer to question `question_id`, or another reply to `asker`
    /// in its thread from someone else (a clarification).
    Answer {
        question_id: String,
        thread_id: String,
        asker: String,
        /// Messages of the thread already accounted for: present when the
        /// wait began, or checked since. Only others can end the wait.
        seen: HashSet<String>,
    },
    /// Unread mail for `addr`.
    Mail { addr: String },
}

impl Wait {
    /// Whether `event` may end this wait, so the store is worth checking.
    pub fn wakes(&self, event: &Event) -> bool {
        match (self, event) {
            (Wait::Answer { thread_id, .. }, Event::Delivered { message, .. }) => {
                message.thread_id == *thread_id
            }
            (Wait::Mail { addr }, Event::Delivered { to, .. }) => to == addr,
            (_, Event::Presence { .. }) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Message};

    fn delivered(to: &str, thread_id: &str) -> Event {
        Event::Delivered {
            to: to.into(),
            message: Message {
                id: "m".into(),
                thread_id: thread_id.into(),
                in_reply_to: None,
                from: "x-1".into(),
                to: vec![to.into()],
                subject: "s".into(),
                body: "b".into(),
                kind: Kind::Note,
                urgent: false,
                refs: vec![],
                created_at: 0,
                from_human: false,
            },
        }
    }

    #[test]
    fn hub_wait_wakes_only_on_relevant_deliveries() {
        let answer = Wait::Answer {
            question_id: "q".into(),
            thread_id: "q".into(),
            asker: "a-1".into(),
            seen: HashSet::new(),
        };
        // Any delivery in the thread, whoever it is for: the answer may be
        // addressed to others as well.
        assert!(answer.wakes(&delivered("a-1", "q")));
        assert!(answer.wakes(&delivered("b-1", "q")));
        assert!(!answer.wakes(&delivered("a-1", "other")));

        let mail = Wait::Mail { addr: "a-1".into() };
        assert!(mail.wakes(&delivered("a-1", "t")));
        assert!(!mail.wakes(&delivered("b-1", "t")));
    }
}
