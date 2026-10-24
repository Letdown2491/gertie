//! gertie-providers: adapts model vendors to gertie's typed event stream.
//!
//! The [`Provider`] trait is gertie's most important API. It is designed
//! around an agent harness's needs — typed events, whole normalized tool
//! calls, usage accounting, cancellation — never around a vendor's wire
//! format. OpenAI's schema is just the first adapter.

use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use gertie_core::Event;
use std::time::Duration;
use tokio_stream::wrappers::UnboundedReceiverStream;

/// One configured endpoint: where to POST, how to authenticate, what model
/// to ask for. Keys resolve from config values or the environment at
/// construction time — never stored in logs or events.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub name: String,
    /// e.g. `https://api.ppq.ai/v1` — `/chat/completions` is appended.
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

/// One model request. The loop fills this; providers translate it.
#[derive(Clone, Debug, Default)]
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

/// Retry policy per DESIGN.md: auto-retry transient failures with
/// exponential backoff + jitter; never auto-retry auth errors.
const MAX_ATTEMPTS: u32 = 3;
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Any OpenAI-compatible chat/completions endpoint (ppq.ai, OpenRouter,
/// Ollama's OpenAI mode, LM Studio, vLLM, …).
pub struct OpenAICompatible {
    endpoint: Endpoint,
    client: reqwest::Client,
    /// Own multi-thread runtime: spawned tasks must make progress without
    /// any caller entering the runtime (the UI polls the returned stream
    /// on gpui's executor instead).
    runtime: tokio::runtime::Runtime,
}

/// Per-request state: reqwest clients are cheap Arc handles; the endpoint
/// is small. Cloning sidesteps lifetime plumbing through the spawned task.
#[derive(Clone)]
struct RequestScoped {
    endpoint: Endpoint,
    client: reqwest::Client,
}

/// What one HTTP attempt decided.
enum Outcome {
    /// Stream completed (however it ended — errors were already sent).
    Done,
    /// Auth/config problem: never retried.
    Fatal(String),
    /// Worth retrying per the policy.
    Transient(String),
}

/// One partially accumulated tool call: OpenAI-compatible servers stream a
/// tool call as many small deltas keyed by index; gertie normalizes them
/// so the loop and UI only ever see whole calls.
#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl OpenAICompatible {
    pub fn new(endpoint: Endpoint) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder().build()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        Ok(Self {
            endpoint,
            client,
            runtime,
        })
    }
}

impl Provider for OpenAICompatible {
    fn stream(&self, request: Request) -> BoxStream<'static, Event> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let scoped = RequestScoped {
            endpoint: self.endpoint.clone(),
            client: self.client.clone(),
        };
        self.runtime.spawn(async move {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                let _ = sender.send(Event::Started);
                match scoped.attempt_once(&request, &sender).await {
                    Outcome::Done => return,
                    Outcome::Fatal(message) => {
                        let _ = sender.send(Event::Error { message });
                        return;
                    }
                    Outcome::Transient(message) => {
                        if attempt >= MAX_ATTEMPTS {
                            let _ = sender.send(Event::Error {
                                message: format!("{message} (after {attempt} attempts)"),
                            });
                            return;
                        }
                        let _ = sender.send(Event::Error {
                            message: format!(
                                "{message} — retrying (attempt {}/{MAX_ATTEMPTS})",
                                attempt + 1
                            ),
                        });
                        tokio::time::sleep(backoff(attempt)).await;
                    }
                }
            }
        });
        Box::pin(UnboundedReceiverStream::new(receiver))
    }
}

impl RequestScoped {
    async fn attempt_once(
        &self,
        request: &Request,
        sender: &tokio::sync::mpsc::UnboundedSender<Event>,
    ) -> Outcome {
        let mut body = serde_json::json!({
            "model": self.endpoint.model,
            "messages": request
                .messages
                .iter()
                .map(|(role, content)| serde_json::json!({
                    "role": wire_role(*role),
                    "content": content,
                }))
                .collect::<Vec<_>>(),
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if let Some(max) = request.max_output_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }

        let mut request = self.client.post(endpoint_url(&self.endpoint)).json(&body);
        if let Some(key) = &self.endpoint.api_key {
            request = request.bearer_auth(key);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => return Outcome::Transient(format!("connection failed: {error}")),
        };

        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Outcome::Fatal(format!("auth rejected ({status}) — check the API key"));
        }
        if status.is_server_error() || status.as_u16() == 429 {
            let detail = response.text().await.unwrap_or_default();
            return Outcome::Transient(format!("{status}: {}", truncate(&detail, 200)));
        }
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Outcome::Fatal(format!("{status}: {}", truncate(&detail, 200)));
        }

        parse_sse(response.bytes_stream(), sender).await
    }
}

/// SSE parse: line-oriented `data:` events; `[DONE]` closes; usage arrives
/// on a final chunk when the server supports include_usage.
async fn parse_sse(
    stream: impl futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>>,
    sender: &tokio::sync::mpsc::UnboundedSender<Event>,
) -> Outcome {
    let mut stream = std::pin::pin!(stream);
    let mut buffer = Vec::new();
    let mut tool_calls: Vec<PartialToolCall> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break; // transport died mid-stream; what landed still stands
        };
        buffer.extend_from_slice(&chunk);
        while let Some(line_end) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=line_end).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(payload) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload == "[DONE]" {
                flush_tool_calls(&tool_calls, sender);
                let _ = sender.send(Event::Done);
                return Outcome::Done;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
                continue;
            };
            if let Some(message) = value.get("error").and_then(|error| error.get("message")) {
                let _ = sender.send(Event::Error {
                    message: message.as_str().unwrap_or("unknown error").to_string(),
                });
                return Outcome::Done;
            }
            emit_delta(&value, &mut tool_calls, sender);
            if let Some(usage) = value.get("usage") {
                if let (Some(input), Some(output)) = (
                    usage.get("prompt_tokens").and_then(count),
                    usage.get("completion_tokens").and_then(count),
                ) {
                    let _ = sender.send(Event::Usage {
                        input_tokens: input,
                        output_tokens: output,
                    });
                }
            }
        }
    }
    // Server closed without [DONE]: flush whatever accumulated.
    flush_tool_calls(&tool_calls, sender);
    let _ = sender.send(Event::Done);
    Outcome::Done
}

fn emit_delta(
    value: &serde_json::Value,
    tool_calls: &mut Vec<PartialToolCall>,
    sender: &tokio::sync::mpsc::UnboundedSender<Event>,
) {
    let Some(delta) = value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("delta"))
    else {
        return;
    };
    if let Some(content) = delta.get("content").and_then(|content| content.as_str()) {
        if !content.is_empty() {
            let _ = sender.send(Event::Token {
                delta: content.to_string(),
            });
        }
    }
    if let Some(calls) = delta.get("tool_calls").and_then(|calls| calls.as_array()) {
        for call in calls {
            let Some(index) = call.get("index").and_then(count).map(|i| i as usize) else {
                continue;
            };
            if tool_calls.len() <= index {
                tool_calls.resize_with(index + 1, PartialToolCall::default);
            }
            let slot = &mut tool_calls[index];
            if let Some(id) = call.get("id").and_then(|id| id.as_str()) {
                slot.id.push_str(id);
            }
            if let Some(function) = call.get("function") {
                if let Some(name) = function.get("name").and_then(|name| name.as_str()) {
                    slot.name.push_str(name);
                }
                if let Some(arguments) = function.get("arguments").and_then(|a| a.as_str()) {
                    slot.arguments.push_str(arguments);
                }
            }
        }
    }
}

fn flush_tool_calls(
    tool_calls: &[PartialToolCall],
    sender: &tokio::sync::mpsc::UnboundedSender<Event>,
) {
    for call in tool_calls {
        if call.name.is_empty() {
            continue;
        }
        let _ = sender.send(Event::ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        });
    }
}

fn wire_role(role: gertie_core::Role) -> &'static str {
    match role {
        gertie_core::Role::System => "system",
        gertie_core::Role::User => "user",
        gertie_core::Role::Assistant => "assistant",
        gertie_core::Role::Tool => "tool",
    }
}

fn endpoint_url(endpoint: &Endpoint) -> String {
    let base = endpoint.base_url.trim_end_matches('/');
    format!("{base}/chat/completions")
}

/// Full jitter: uniform in [0, 2^(attempt-1) * base). Clock-derived
/// randomness keeps this dependency-free; precision of the distribution
/// does not matter for a retry delay.
fn backoff(attempt: u32) -> Duration {
    let max = INITIAL_BACKOFF.as_millis() as u64 * (1 << (attempt - 1).min(16));
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        % (max + 1);
    Duration::from_millis(jitter)
}

fn count(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_i64().map(|n| n.max(0) as u64))
}

fn truncate(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_core::Stream;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::task::{Context as TaskContext, Poll, Waker};

    /// Drains a stream to completion on the current thread. The provider's
    /// tokio tasks run on their own runtime; this only polls the receiving
    /// end, so `Pending` here means "task still producing", not deadlock.
    fn drain(mut stream: BoxStream<'static, Event>) -> Vec<Event> {
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);
        let mut events = Vec::new();
        loop {
            match std::pin::Pin::new(&mut stream).poll_next(&mut cx) {
                Poll::Ready(Some(event)) => events.push(event),
                Poll::Ready(None) => return events,
                Poll::Pending => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    /// A fake OpenAI-compatible server: responds to one POST with raw
    /// bytes (already-shaped SSE, or an error status), then closes.
    struct FakeServer {
        address: String,
    }

    impl FakeServer {
        fn start(response: &str) -> Self {
            Self::with_status("200 OK", response)
        }

        fn with_status(status_line: &str, body: &str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let address = listener.local_addr().expect("addr").to_string();
            let response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: text/event-stream\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            std::thread::spawn(move || {
                if let Ok((mut socket, _)) = listener.accept() {
                    let mut buffer = [0u8; 8192];
                    let _ = Read::read(&mut socket, &mut buffer);
                    socket.write_all(response.as_bytes()).unwrap();
                }
            });
            Self { address }
        }

        fn endpoint(&self) -> Endpoint {
            Endpoint {
                name: "fake".into(),
                base_url: format!("http://{}/v1", self.address),
                model: "fake-model".into(),
                api_key: Some("fake-key".into()),
            }
        }
    }

    fn user_request() -> Request {
        Request {
            messages: vec![(gertie_core::Role::User, "hi".into())],
            max_output_tokens: None,
        }
    }

    #[test]
    fn streams_tokens_usage_and_done_from_sse() {
        let server = FakeServer::start(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n\
             data: [DONE]\n\n",
        );
        let provider = OpenAICompatible::new(server.endpoint()).unwrap();
        let events = drain(provider.stream(user_request()));

        assert!(events.contains(&Event::Token {
            delta: "Hel".into()
        }));
        assert!(events.contains(&Event::Token {
            delta: "lo".into()
        }));
        assert!(events.contains(&Event::Usage {
            input_tokens: 7,
            output_tokens: 2,
        }));
        assert_eq!(events.last(), Some(&Event::Done));
    }

    #[test]
    fn split_tool_call_deltas_arrive_whole() {
        let server = FakeServer::start(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"rea\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"d_file\",\"arguments\":\"th\\\":\\\"x\\\"}\"}}]}}]}\n\n\
             data: [DONE]\n\n",
        );
        let provider = OpenAICompatible::new(server.endpoint()).unwrap();
        let events = drain(provider.stream(user_request()));

        assert_eq!(
            events,
            vec![
                Event::Started,
                Event::ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: "{\"path\":\"x\"}".into(),
                },
                Event::Done,
            ]
        );
    }

    #[test]
    fn auth_errors_are_fatal_not_retried() {
        let server = FakeServer::with_status("401 Unauthorized", "{}");
        let provider = OpenAICompatible::new(server.endpoint()).unwrap();
        let events = drain(provider.stream(user_request()));

        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], Event::Started));
        assert!(
            matches!(&events[1], Event::Error { message } if message.contains("auth rejected"))
        );
    }

    #[test]
    fn transient_errors_retry_then_report_the_exhausted_policy() {
        let server = FakeServer::with_status("503 Service Unavailable", "overloaded");
        let provider = OpenAICompatible::new(server.endpoint()).unwrap();
        let events = drain(provider.stream(user_request()));

        let errors: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                Event::Error { message } => Some(message),
                _ => None,
            })
            .collect();
        assert!(!errors.is_empty());
        assert!(errors.last().unwrap().contains("after 3 attempts"));
    }
}
