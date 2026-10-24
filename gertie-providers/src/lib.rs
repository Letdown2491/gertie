//! gertie-providers: adapts model vendors to gertie's typed event stream.
//!
//! The [`Provider`] trait is gertie's most important API. It is designed
//! around an agent harness's needs — typed events, whole normalized tool
//! calls, usage accounting, cancellation — never around a vendor's wire
//! format. OpenAI's schema is just the first adapter.
//!
//! The OpenAI-compatible implementation (covering ppq.ai, OpenRouter, Ollama,
//! LM Studio, vLLM, …) arrives in M1 over an SSE stream.

use futures_core::stream::BoxStream;
use gertie_core::Event;

/// One model request. The loop fills this; providers translate it.
#[derive(Clone, Debug)]
pub struct Request {
    /// Canonical message path, oldest first.
    pub messages: Vec<(gertie_core::Role, String)>,
    /// Maximum output tokens the model should produce, when the provider
    /// supports the knob.
    pub max_output_tokens: Option<u64>,
}

/// A source of model completions. Implementations must own their retry
/// policy (backoff + jitter on 429/5xx/connection errors, never on
/// 401/403) and surface every retry to the event stream before failing.
pub trait Provider {
    /// Streams one response as a sequence of [`Event`]s. The stream always
    /// ends with `Event::Done` or `Event::Error`. Dropping the stream
    /// cancels the request.
    fn stream(&self, request: Request) -> BoxStream<'static, Event>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_core::Stream;
    use std::task::{Context as TaskContext, Poll, Waker};

    /// Drains an already-ready stream to completion on the current thread.
    /// Real streaming runs under an executor from M1 onward; the trait
    /// contract itself needs nothing more than a no-op waker.
    fn drain(stream: &mut (impl Stream<Item = Event> + Unpin)) -> Vec<Event> {
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);
        let mut events = Vec::new();
        loop {
            match std::pin::Pin::new(&mut *stream).poll_next(&mut cx) {
                Poll::Ready(Some(event)) => events.push(event),
                Poll::Ready(None) => return events,
                Poll::Pending => panic!("test stream must not pend"),
            }
        }
    }

    /// Pin the contract: a provider echoes a full response as
    /// Started → Tokens → Done, with tool calls arriving whole.
    #[test]
    fn a_provider_streams_typed_events() {
        struct Echo;

        impl Provider for Echo {
            fn stream(&self, request: Request) -> BoxStream<'static, Event> {
                let text = request
                    .messages
                    .last()
                    .map(|(_, content)| content.clone())
                    .unwrap_or_default();
                Box::pin(futures_util::stream::iter(vec![
                    Event::Started,
                    Event::Token { delta: text },
                    Event::Done,
                ]))
            }
        }

        let mut stream = Echo.stream(Request {
            messages: vec![(gertie_core::Role::User, "hello".into())],
            max_output_tokens: None,
        });
        assert_eq!(
            drain(&mut stream),
            vec![
                Event::Started,
                Event::Token {
                    delta: "hello".into()
                },
                Event::Done,
            ]
        );
    }
}
