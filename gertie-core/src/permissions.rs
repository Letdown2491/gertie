//! The permission broker: one shared registry of pending approval
//! requests, so prompts from *any* loop — the leader's or a worker's —
//! reach the same UI.
//!
//! Why this exists: the leader's loop communicates prompts by yielding
//! `ToolCallRequested`, which works because the UI drains that stream.
//! A subagent's loop runs *inside a tool* — nobody drains its stream.
//! Both loops register asks here; the UI subscribes once and answers by
//! request id. The loop still holds a plain oneshot receiver and awaits
//! it exactly as before — the broker is the queue in front.
//!
//! A dropped request (no answer ever) resolves as a deny in the loop:
//! when the UI goes away, doing nothing is the safe move.

use crate::PermissionDecision;
use crate::request::ToolCall;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Who asked. The UI renders leader asks from the leader's event stream
/// and worker asks from the broker subscription; the tag keeps them
/// distinguishable for labeling (and for tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptSource {
    Leader,
    Worker,
}

/// A pending ask the UI can see: id for answering, the call for display.
#[derive(Clone, Debug)]
pub struct PendingAsk {
    pub id: u64,
    pub call: ToolCall,
    pub source: PromptSource,
}

pub struct PermissionBroker {
    state: Arc<Mutex<BrokerState>>,
    /// Worker asks fan out to every subscriber — one per surface
    /// (multiple tabs, a panel, a headless monitor).
    subscribers: Mutex<Vec<tokio::sync::mpsc::UnboundedSender<PendingAsk>>>,
}

struct BrokerState {
    next_id: u64,
    pending: Vec<(u64, oneshot::Sender<PermissionDecision>)>,
}

impl Default for PermissionBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionBroker {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(BrokerState {
                next_id: 1,
                pending: Vec::new(),
            })),
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// Registers an ask and returns its id plus the receiver the loop
    /// awaits. Worker asks are also published to every subscription.
    pub fn request(
        &self,
        call: ToolCall,
        source: PromptSource,
    ) -> (u64, oneshot::Receiver<PermissionDecision>) {
        let (respond, answer) = oneshot::channel();
        let id = {
            let mut state = self.state.lock().unwrap();
            let id = state.next_id;
            state.next_id += 1;
            state.pending.push((id, respond));
            id
        };
        if source == PromptSource::Worker {
            let mut subscribers = self.subscribers.lock().unwrap();
            // Retain only live subscribers; a surface that went away
            // takes its ask cards with it.
            subscribers.retain(|subscriber| {
                subscriber
                    .send(PendingAsk {
                        id,
                        call: call.clone(),
                        source,
                    })
                    .is_ok()
            });
        }
        (id, answer)
    }

    /// The UI answers by id. Returns false if the id is unknown or
    /// already answered (double click) — harmless either way.
    pub fn respond(&self, id: u64, decision: PermissionDecision) -> bool {
        let sender = {
            let mut state = self.state.lock().unwrap();
            let index = state.pending.iter().position(|(pending, _)| *pending == id);
            match index {
                Some(index) => state.pending.remove(index).1,
                None => return false,
            }
        };
        sender.send(decision).is_ok()
    }

    /// A stream of worker asks. Any number of surfaces subscribe; each
    /// sees every worker ask. No backlog by design: the registry holds
    /// the pending ones, and a surface answers the cards it has.
    pub fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<PendingAsk> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }
    }

    #[test]
    fn asks_round_trip_through_ids() {
        let broker = PermissionBroker::new();
        let (id, answer) = broker.request(call(), PromptSource::Leader);

        assert!(broker.respond(id, PermissionDecision::Approve));
        assert_eq!(answer.blocking_recv(), Ok(PermissionDecision::Approve));
        // Second answer for the same id: unknown, reports false.
        assert!(!broker.respond(id, PermissionDecision::Deny));
    }

    #[test]
    fn worker_asks_publish_to_the_subscription() {
        let broker = PermissionBroker::new();
        let mut rx = broker.subscribe();
        let (id, _answer) = broker.request(call(), PromptSource::Worker);

        let ask = rx.blocking_recv().expect("published");
        assert_eq!(ask.id, id);
        assert_eq!(ask.source, PromptSource::Worker);
    }

    #[test]
    fn every_surface_subscribes_and_sees_every_ask() {
        let broker = PermissionBroker::new();
        let mut first = broker.subscribe();
        let mut second = broker.subscribe();
        let (id, _answer) = broker.request(call(), PromptSource::Worker);

        assert_eq!(first.blocking_recv().unwrap().id, id);
        assert_eq!(second.blocking_recv().unwrap().id, id);
        // Dropping a surface doesn't break the others: the broker
        // prunes dead subscribers on publish.
        drop(first);
        let (id2, _answer2) = broker.request(call(), PromptSource::Worker);
        assert_eq!(second.blocking_recv().unwrap().id, id2);
    }

    #[test]
    fn leader_asks_stay_off_the_subscription() {
        let broker = PermissionBroker::new();
        let mut rx = broker.subscribe();
        let (id, answer) = broker.request(call(), PromptSource::Leader);
        broker.respond(id, PermissionDecision::Deny);
        // The loop-side receiver still resolves.
        assert_eq!(answer.blocking_recv(), Ok(PermissionDecision::Deny));
        // Nothing was published: the UI saw the ask via the event.
        assert!(rx.try_recv().is_err());
    }
}
