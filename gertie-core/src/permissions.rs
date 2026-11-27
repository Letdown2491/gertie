//! The permission broker: one shared registry of pending approval
//! requests, so prompts from *any* loop, the leader's or a worker's,
//! reach the same UI.
//!
//! Why this exists: the leader's loop communicates prompts by yielding
//! `ToolCallRequested`, which works because the UI drains that stream.
//! A subagent's loop runs *inside a tool*, nobody drains its stream.
//! Both loops register asks here; the UI subscribes once and answers by
//! request id. The loop still holds a plain oneshot receiver and awaits
//! it exactly as before, the broker is the queue in front.
//!
//! A dropped request (no answer ever) resolves as a deny in the loop:
//! when the UI goes away, doing nothing is the safe move.

use crate::PermissionDecision;
use crate::request::ToolCall;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Identifies the turn a worker ask belongs to. The UI's live turns
/// each hold one (minted at turn start, carried on the `TurnSpec`);
/// the broker routes a worker ask to the subscriber whose token
/// matches. An ask with no token (a path that predates the field, or
/// a caller that never learned of it) is unscoped and fans out to
/// every subscriber, the behavior before this field existed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnToken(pub u64);

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
    /// The turn the ask belongs to, when the asker knew it. A Some
    /// token routes the ask to that turn's surface alone; None falls
    /// back to every subscriber.
    pub turn: Option<TurnToken>,
}

/// A subscription's filter, minted by [`PermissionBroker::subscribe`].
/// A scoped subscriber receives only the asks carrying its token (plus
/// unscoped ones, the token-absent fallback); an unscoped one sees
/// every worker ask, exactly as before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskFilter {
    /// Every surface that predates turn scoping: the headless pump,
    /// tests, a monitor that wants the whole firehose.
    EveryWorkerAsk,
    /// One chat's live turn: this surface spawned the worker, the ask
    /// is nobody else's business.
    Turn(TurnToken),
}

pub struct PermissionBroker {
    state: Arc<Mutex<BrokerState>>,
    /// Worker asks fan out to their surfaces, one subscription per
    /// surface (multiple tabs, a panel, a headless monitor), each with
    /// its filter.
    subscribers: Mutex<Vec<(AskFilter, tokio::sync::mpsc::UnboundedSender<PendingAsk>)>>,
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
    /// awaits. Worker asks are also published to the subscription: to
    /// the turn's surface when the ask carries a turn token, to every
    /// surface when it does not (the token-absent fallback).
    pub fn request(
        &self,
        call: ToolCall,
        source: PromptSource,
        turn: Option<TurnToken>,
    ) -> (u64, oneshot::Receiver<PermissionDecision>) {
        let (respond, answer) = oneshot::channel();
        let id = {
            let mut state = self.state.lock().unwrap();
            // Asks whose asker is gone (a stopped turn, a closed view)
            // can never be answered: their entries would only pile up.
            state.pending.retain(|(_, sender)| !sender.is_closed());
            let id = state.next_id;
            state.next_id += 1;
            state.pending.push((id, respond));
            id
        };
        if source == PromptSource::Worker {
            let ask = PendingAsk {
                id,
                call: call.clone(),
                source,
                turn,
            };
            let mut subscribers = self.subscribers.lock().unwrap();
            // Retain only live subscribers; a surface that went away
            // takes its ask cards with it.
            subscribers.retain(|(filter, subscriber)| {
                let routed = match (turn, filter) {
                    // The firehose catches only what no address can
                    // name: an ask that predates scoping cannot name
                    // its turn, so it stays everyone's, never silently
                    // swallowed. A stamped ask is addressed; a firehose
                    // subscriber (headless, tests) does not receive
                    // other turns' workers (issue #15).
                    (None, AskFilter::EveryWorkerAsk) => true,
                    (None, _) => true,
                    (Some(token), AskFilter::Turn(own)) => token == *own,
                    (Some(_), AskFilter::EveryWorkerAsk) => false,
                };
                routed && subscriber.send(ask.clone()).is_ok()
            });
        }
        (id, answer)
    }

    /// The UI answers by id. Returns false if the id is unknown or
    /// already answered (double click), harmless either way.
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

    /// A stream of worker asks. Any number of surfaces subscribe; a
    /// scoped one sees only its turn's asks (plus unscoped ones, the
    /// token-absent fallback), an unscoped one sees every worker ask.
    /// No backlog by design: the registry holds the pending ones, and
    /// a surface answers the cards it has.
    pub fn subscribe(&self, filter: AskFilter) -> tokio::sync::mpsc::UnboundedReceiver<PendingAsk> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.subscribers.lock().unwrap().push((filter, tx));
        rx
    }

    /// Mint a turn token. Called once per turn at turn start; the
    /// same u64 rides the `TurnSpec` and this surface's filter.
    pub fn next_turn_token(&self) -> TurnToken {
        let mut state = self.state.lock().unwrap();
        let token = TurnToken(state.next_id);
        state.next_id += 1;
        token
    }
}

/// How a turn's mutating-tool asks resolve. A property of the turn,
/// carried on the `TurnSpec`, fixed for its whole life: flipping the
/// mode mid-turn would change the rules under a running agent, so a
/// flip applies to the next turn instead.
///
/// The hard base layer sits below every mode: the tools' own policy
/// vet (destructive commands) blocks before any ask could happen, and
/// a mode never re-approves what the vet refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Every mutating tool asks. The default: the prompt gate is the
    /// base layer.
    #[default]
    Supervised,
    /// Edit-class work runs unasked, exactly what the checkpoint hook
    /// snapshots, so every auto-approved change is reversible. Bash
    /// still asks: it touches the world outside any single file.
    AcceptEdits,
    /// Edits as in AcceptEdits, plus bash that runs inside the Landlock
    /// sandbox, contained, so contained work needs no chaperone. With
    /// the sandbox off, this mode asks like AcceptEdits; it never
    /// pretends a kernel that isn't enforcing is enforcing. A tool's
    /// own contained calls (`Tool::contained`: a screenshot of a
    /// workspace page or a local dev server) run unasked here too.
    Auto,
    /// Nothing asks. The vet stays; the human is the only thing
    /// removed from the path.
    FullAccess,
}

impl PermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionMode::Supervised => "supervised",
            PermissionMode::AcceptEdits => "accept_edits",
            PermissionMode::Auto => "auto",
            PermissionMode::FullAccess => "full_access",
        }
    }

    /// Whether this mode settles a mutating-tool ask without the
    /// broker. `checkpointed`, the checkpoint hook will snapshot
    /// what the call touches, so the change is reversible;
    /// `contained`, this exact call is bash running inside the
    /// Landlock sandbox (a per-call fact: the sandbox contains bash,
    /// not whatever else the turn does); `third_party`, an MCP tool,
    /// whose side effects gertie cannot vouch for. The reversibility
    /// and containment arguments only cover gertie's own tools: a
    /// third-party tool's notion of a "path" is its own, so neither
    /// ever pre-approves one, every mode short of full_access asks.
    pub fn auto_allows(&self, checkpointed: bool, contained: bool, third_party: bool) -> bool {
        match self {
            PermissionMode::Supervised => false,
            PermissionMode::AcceptEdits => checkpointed && !third_party,
            PermissionMode::Auto => (checkpointed || contained) && !third_party,
            PermissionMode::FullAccess => true,
        }
    }
}

impl std::str::FromStr for PermissionMode {
    type Err = ();

    /// The mode a stored row or config key names. Unknown values (a
    /// hand-edited database) read as Err; callers fall back to the
    /// default.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "supervised" => Ok(PermissionMode::Supervised),
            "accept_edits" => Ok(PermissionMode::AcceptEdits),
            "auto" => Ok(PermissionMode::Auto),
            "full_access" => Ok(PermissionMode::FullAccess),
            _ => Err(()),
        }
    }
}

/// The front's active permission mode, shared so worker dispatch can
/// inherit the leader's turn mode (a worker's `TurnSpec` is built
/// inside the agent tool, which sees no turn of its own). Set once at
/// turn start, read at dispatch, never mutated mid-turn.
#[derive(Clone, Default)]
pub struct ModeScope(std::sync::Arc<std::sync::atomic::AtomicU8>);

impl ModeScope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, mode: PermissionMode) {
        self.0.store(
            match mode {
                PermissionMode::Supervised => 0,
                PermissionMode::AcceptEdits => 1,
                PermissionMode::Auto => 2,
                PermissionMode::FullAccess => 3,
            },
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub fn get(&self) -> PermissionMode {
        match self.0.load(std::sync::atomic::Ordering::Relaxed) {
            1 => PermissionMode::AcceptEdits,
            2 => PermissionMode::Auto,
            3 => PermissionMode::FullAccess,
            _ => PermissionMode::Supervised,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Auto's containment argument is per-call: true means this exact
    /// call is bash inside the Landlock sandbox. Containment never
    /// extends the auto-approval to third-party code, whose side
    /// effects and path notions gertie cannot vouch for: a
    /// sandbox-looking turn must not wave an MCP tool through.
    #[test]
    fn auto_never_pre_approves_third_party_even_contained() {
        let auto = PermissionMode::Auto;
        assert!(
            !auto.auto_allows(false, true, true),
            "the QA finding: a turn-wide sandbox flag waved MCP tools through"
        );
        assert!(!auto.auto_allows(true, true, true));
        // Contained bash and checkpointed own tools still pre-approve.
        assert!(auto.auto_allows(false, true, false));
        assert!(auto.auto_allows(true, false, false));
        // Accept-edits: checkpointed own tools only, containment adds
        // nothing there.
        assert!(!PermissionMode::AcceptEdits.auto_allows(false, true, false));
        assert!(PermissionMode::AcceptEdits.auto_allows(true, false, false));
        // Supervised never pre-approves anything.
        assert!(!PermissionMode::Supervised.auto_allows(true, true, false));
        assert!(!PermissionMode::Supervised.auto_allows(false, true, true));
    }

    /// A stopped turn drops its receiver mid-ask: the next request
    /// prunes the dead entry, and answering it is a harmless no-op.
    #[test]
    fn abandoned_asks_do_not_pile_up() {
        let broker = PermissionBroker::new();
        let (stale, receiver) = broker.request(call(), PromptSource::Leader, None);
        drop(receiver);
        let (live, _receiver) = broker.request(call(), PromptSource::Leader, None);
        assert_eq!(broker.state.lock().unwrap().pending.len(), 1);
        assert!(!broker.respond(stale, PermissionDecision::Approve));
        assert!(broker.respond(live, PermissionDecision::Approve));
    }

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
        let (id, answer) = broker.request(call(), PromptSource::Leader, None);

        assert!(broker.respond(id, PermissionDecision::Approve));
        assert_eq!(answer.blocking_recv(), Ok(PermissionDecision::Approve));
        // Second answer for the same id: unknown, reports false.
        assert!(!broker.respond(id, PermissionDecision::Deny { feedback: None }));
    }

    #[test]
    fn worker_asks_publish_to_the_subscription() {
        let broker = PermissionBroker::new();
        let mut rx = broker.subscribe(AskFilter::EveryWorkerAsk);
        let (id, _answer) = broker.request(call(), PromptSource::Worker, None);

        let ask = rx.blocking_recv().expect("published");
        assert_eq!(ask.id, id);
        assert_eq!(ask.source, PromptSource::Worker);
    }

    #[test]
    fn a_turn_scoped_ask_reaches_its_own_surface_and_no_other() {
        let broker = PermissionBroker::new();
        let token = broker.next_turn_token();
        let other = broker.next_turn_token();
        let mut ours = broker.subscribe(AskFilter::Turn(token));
        let mut theirs = broker.subscribe(AskFilter::Turn(other));
        let mut firehose = broker.subscribe(AskFilter::EveryWorkerAsk);
        let (id, _answer) = broker.request(call(), PromptSource::Worker, Some(token));

        // The owning surface gets it, the other turn's surface does
        // not, and the firehose does not either: a stamped ask is
        // addressed (issue #15). The firehose catches only unstamped
        // asks, the resume fallback.
        let ask = ours.blocking_recv().expect("published to the owner");
        assert_eq!(ask.id, id);
        assert_eq!(ask.turn, Some(token));
        assert!(
            theirs.try_recv().is_err(),
            "a foreign turn never sees the ask"
        );
        assert!(
            firehose.try_recv().is_err(),
            "a stamped ask is addressed, not broadcast"
        );
    }

    #[test]
    fn an_unscoped_ask_falls_back_to_every_turn_scoped_subscriber() {
        let broker = PermissionBroker::new();
        let token = broker.next_turn_token();
        let mut first = broker.subscribe(AskFilter::Turn(token));
        let mut second = broker.subscribe(AskFilter::Turn(broker.next_turn_token()));
        let (id, _answer) = broker.request(call(), PromptSource::Worker, None);

        // The token-absent fallback: the ask predates scoping (or the
        // path never set one), it is everyone's, never swallowed.
        assert_eq!(first.blocking_recv().unwrap().id, id);
        assert_eq!(second.blocking_recv().unwrap().id, id);
    }

    #[test]
    fn every_surface_subscribes_and_sees_every_ask() {
        let broker = PermissionBroker::new();
        let mut first = broker.subscribe(AskFilter::EveryWorkerAsk);
        let mut second = broker.subscribe(AskFilter::EveryWorkerAsk);
        let (id, _answer) = broker.request(call(), PromptSource::Worker, None);

        assert_eq!(first.blocking_recv().unwrap().id, id);
        assert_eq!(second.blocking_recv().unwrap().id, id);
        // Dropping a surface doesn't break the others: the broker
        // prunes dead subscribers on publish.
        drop(first);
        let (id2, _answer2) = broker.request(call(), PromptSource::Worker, None);
        assert_eq!(second.blocking_recv().unwrap().id, id2);
    }

    #[test]
    fn leader_asks_stay_off_the_subscription() {
        let broker = PermissionBroker::new();
        let mut rx = broker.subscribe(AskFilter::EveryWorkerAsk);
        let (id, answer) = broker.request(call(), PromptSource::Leader, None);
        broker.respond(id, PermissionDecision::Deny { feedback: None });
        // The loop-side receiver still resolves.
        assert_eq!(
            answer.blocking_recv(),
            Ok(PermissionDecision::Deny { feedback: None })
        );
        // Nothing was published: the UI saw the ask via the event.
        assert!(rx.try_recv().is_err());
    }
}
