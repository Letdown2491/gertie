//! gertie-providers: adapts model vendors to gertie's typed event stream.
//!
//! The [`Provider`] trait is gertie's most important API. It is designed
//! around an agent harness's needs (typed events, whole normalized tool
//! calls, usage accounting, cancellation), never around a vendor's wire
//! format. OpenAI's schema is just the first adapter.

use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use gertie_core::Event;
use gertie_core::request::{Request, WireMessage};
use std::time::Duration;

// The Provider trait and Request shape are core concepts (the loop drives
// them directly); this crate only implements them.
pub use gertie_core::{Provider, ToolCall, ToolSpec};

/// Which wire protocol an endpoint speaks. OpenAI's chat/completions
/// schema is the default and the most universal (ppq.ai, OpenRouter,
/// Ollama, vLLM); Anthropic's Messages API is the first second adapter:
/// the honest test of whether the Provider trait is really
/// vendor-neutral; Replay is the dev/test wire that streams scripted
/// scenarios through the real loop.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Wire {
    #[default]
    OpenAI,
    Anthropic,
    Replay,
}

impl Wire {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "openai" => Some(Self::OpenAI),
            "anthropic" => Some(Self::Anthropic),
            "replay" => Some(Self::Replay),
            _ => None,
        }
    }
}

/// One configured endpoint: where to POST, how to authenticate, what model
/// to ask for. Keys resolve from config values or the environment at
/// construction time, never stored in logs or events.
#[derive(Clone)]
pub struct Endpoint {
    pub name: String,
    /// e.g. `https://api.ppq.ai/v1`: the wire appends its own path
    /// (`/chat/completions`, `/v1/messages`).
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    /// Model context window in tokens; None disables compaction.
    pub context_window: Option<u64>,
    pub wire: Wire,
    /// Whether the model reads images (#50). Off: every image in the
    /// request (an attachment, a screenshot) is replaced by a note
    /// saying so, rather than pixels the endpoint rejects or the model
    /// silently ignores. The tools that make images name their files
    /// in the text, so the model still knows where they are.
    pub vision: bool,
}

// Debug is hand-rolled for one reason: the key. A struct that carries
// a secret must not print it because someone logged the request
// pipeline. No `{:?}` of an Endpoint ever shows more than the key's
// presence.
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "…"))
            .field("context_window", &self.context_window)
            .field("wire", &self.wire)
            .field("vision", &self.vision)
            .finish()
    }
}

/// Retry policy per DESIGN.md: auto-retry transient failures with
/// exponential backoff + jitter; never auto-retry auth errors.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

pub mod anthropic;
mod envelope;
pub mod replay;

pub use anthropic::Anthropic;
pub use replay::ReplayProvider;

/// The wire adapters, behind one type. The loop and UI take this;
/// which wire a given endpoint speaks is decided once, at construction.
pub enum AnyProvider {
    OpenAI(OpenAICompatible),
    Anthropic(Anthropic),
    Replay(ReplayProvider),
}

impl AnyProvider {
    pub fn context_window(&self) -> Option<u64> {
        match self {
            Self::OpenAI(provider) => provider.context_window(),
            Self::Anthropic(provider) => provider.context_window(),
            Self::Replay(provider) => provider.context_window(),
        }
    }

    pub fn with_endpoints(endpoints: Vec<Endpoint>) -> Result<Self, reqwest::Error> {
        match endpoints
            .first()
            .map(|endpoint| endpoint.wire.clone())
            .unwrap_or_default()
        {
            Wire::OpenAI => OpenAICompatible::with_endpoints(endpoints).map(Self::OpenAI),
            Wire::Anthropic => Anthropic::with_endpoints(endpoints).map(Self::Anthropic),
            Wire::Replay => ReplayProvider::with_endpoints(endpoints).map(Self::Replay),
        }
    }
}

impl gertie_core::Provider for AnyProvider {
    fn stream(&self, request: Request) -> BoxStream<'static, Event> {
        match self {
            Self::OpenAI(provider) => provider.stream(request),
            Self::Anthropic(provider) => provider.stream(request),
            Self::Replay(provider) => provider.stream(request),
        }
    }
}

/// Any OpenAI-compatible chat/completions endpoint (ppq.ai, OpenRouter,
/// Ollama's OpenAI mode, LM Studio, vLLM, …), plus optional fallbacks.
pub struct OpenAICompatible {
    /// Ordered: [0] is preferred; the rest are tried in sequence when an
    /// endpoint dies before producing any content. Once an endpoint
    /// streams content the turn is committed to it. A model switch
    /// mid-reply would orphan what the user already saw.
    endpoints: Vec<Endpoint>,
}

impl OpenAICompatible {
    /// Configured context window (the preferred endpoint's), for the
    /// loop's compaction trigger.
    pub fn context_window(&self) -> Option<u64> {
        self.endpoints
            .first()
            .and_then(|endpoint| endpoint.context_window)
    }
}

/// Per-request state: reqwest clients are cheap Arc handles; the endpoint
/// is small. Cloning sidesteps lifetime plumbing through the spawned task.
#[derive(Clone)]
pub(crate) struct RequestScoped {
    pub(crate) endpoint: Endpoint,
    pub(crate) client: reqwest::Client,
}

/// What one HTTP attempt decided.
pub(crate) enum Outcome {
    /// Stream completed (however it ended, errors were already sent).
    Done,
    /// Auth/config problem: never retried.
    Fatal(String),
    /// Worth retrying per the policy.
    Transient(String),
    /// The usage/rate limit refused the call (HTTP 429). Retried like a
    /// transient (brief blips recover), but the terminal outcome is
    /// Limited, not Error: the turn ends with a Resume affordance, not
    /// a failure report.
    Limited(String),
}

/// Why the retry policy gave up. The distinction is the UI's: an error
/// is a report; a limit ends the turn in a persisted Limited state
/// with a manual Resume.
pub(crate) enum Terminal {
    Error(String),
    Limited(String),
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
        Self::with_endpoints(vec![endpoint])
    }

    /// Preferred endpoint plus fallbacks. When an endpoint fails before
    /// producing any content (auth rejected, connection refused, 5xx
    /// after retries), the next endpoint is tried, invisibly: an
    /// Event::Error is terminal for the turn (the loop stops on the
    /// first one), so nothing is emitted until every endpoint is
    /// exhausted.
    pub fn with_endpoints(endpoints: Vec<Endpoint>) -> Result<Self, reqwest::Error> {
        Ok(Self { endpoints })
    }
}

impl Provider for OpenAICompatible {
    fn stream(&self, request: Request) -> BoxStream<'static, Event> {
        envelope::stream(
            self.endpoints.clone(),
            request,
            |scoped, request, sender| Box::pin(scoped.attempt_once(request, sender)),
        )
    }
}

impl RequestScoped {
    async fn attempt_once(&self, request: &Request, sender: &envelope::Sender) -> Outcome {
        let mut body = serde_json::json!({
            "model": self.endpoint.model,
            "messages": wire_messages(&readable_by(&self.endpoint, request).messages),
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if let Some(max) = request.max_output_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::json!(
                request
                    .tools
                    .iter()
                    .map(|spec| serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": spec.name,
                            "description": spec.description,
                            "parameters": spec.parameters,
                        },
                    }))
                    .collect::<Vec<_>>()
            );
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
            return Outcome::Fatal(format!("auth rejected ({status}): check the API key"));
        }
        // The rate limit is a 4xx, not a server error, but it gets its
        // own outcome: retried like a transient, terminal as Limited.
        if status.as_u16() == 429 {
            let detail = response.text().await.unwrap_or_default();
            return Outcome::Limited(format!(
                "{status}: {}",
                gertie_core::text::prefix_chars(&detail, 200)
            ));
        }
        if status.is_server_error() {
            let detail = response.text().await.unwrap_or_default();
            return Outcome::Transient(format!(
                "{status}: {}",
                gertie_core::text::prefix_chars(&detail, 200)
            ));
        }
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Outcome::Fatal(format!(
                "{status}: {}",
                gertie_core::text::prefix_chars(&detail, 200)
            ));
        }

        parse_sse(response.bytes_stream(), sender).await
    }
}

/// SSE parse: line-oriented `data:` events; `[DONE]` closes; usage arrives
/// on a final chunk when the server supports include_usage.
async fn parse_sse(
    stream: impl futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>>,
    sender: &envelope::Sender,
) -> Outcome {
    let mut stream = std::pin::pin!(stream);
    let mut lines = envelope::SseLines::default();
    let mut tool_calls: Vec<PartialToolCall> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break; // transport died mid-stream; what landed still stands
        };
        if let Err(message) = lines.push(&chunk) {
            let _ = sender.send(Event::Error { message });
            return Outcome::Done;
        }
        while let Some(line) = lines.next_line() {
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
            if let Some(usage) = value.get("usage")
                && let (Some(input), Some(output)) = (
                    usage.get("prompt_tokens").and_then(count),
                    usage.get("completion_tokens").and_then(count),
                )
            {
                // prompt_tokens_details.cached_tokens is a subset of
                // prompt_tokens on this wire; absent on providers that
                // do not pass cache fields through (stays 0).
                let cached = usage
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(count)
                    .unwrap_or(0);
                let _ = sender.send(Event::Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cached_input_tokens: cached,
                });
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
    sender: &envelope::Sender,
) {
    let Some(delta) = value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("delta"))
    else {
        return;
    };
    if let Some(content) = delta.get("content").and_then(|content| content.as_str())
        && !content.is_empty()
    {
        let _ = sender.send(Event::Token {
            delta: content.to_string(),
        });
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

fn flush_tool_calls(tool_calls: &[PartialToolCall], sender: &envelope::Sender) {
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

/// The request as this endpoint can read it (#50): unchanged for a
/// model with vision, and for one without, every image swapped for a
/// note in the message's text. A request with no images borrows.
pub(crate) fn readable_by<'a>(
    endpoint: &Endpoint,
    request: &'a Request,
) -> std::borrow::Cow<'a, Request> {
    let is_image = |attachment: &String| {
        gertie_core::data_url_parts(attachment)
            .is_some_and(|(media_type, _)| media_type.starts_with("image/"))
    };
    let carries_images = request
        .messages
        .iter()
        .any(|message| message.attachments.iter().any(is_image));
    if endpoint.vision || !carries_images {
        return std::borrow::Cow::Borrowed(request);
    }
    let mut request = request.clone();
    for message in &mut request.messages {
        let before = message.attachments.len();
        message
            .attachments
            .retain(|attachment| !is_image(attachment));
        let dropped = before - message.attachments.len();
        if dropped > 0 {
            let note = format!(
                "[{dropped} image{} not sent: this model can't read images]",
                if dropped == 1 { "" } else { "s" }
            );
            let content = message.content.get_or_insert_with(String::new);
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&note);
        }
    }
    std::borrow::Cow::Owned(request)
}

fn wire_role(role: gertie_core::Role) -> &'static str {
    match role {
        gertie_core::Role::System => "system",
        gertie_core::Role::User => "user",
        gertie_core::Role::Assistant => "assistant",
        gertie_core::Role::Tool => "tool",
    }
}

/// Canonical → OpenAI wire. Three shapes matter beyond plain turns:
/// assistant-with-tool-calls (`tool_calls` array), tool results (keyed by
/// `tool_call_id`), and the tool definitions themselves.
fn wire_messages(messages: &[WireMessage]) -> Vec<serde_json::Value> {
    // A tool message carries text only on this wire, so a tool's
    // images (#50) ride in one user message after the run of tool
    // results: the results must follow their assistant message
    // unbroken, and a user message is where image parts are allowed.
    let mut wire = Vec::new();
    let mut pending: Vec<serde_json::Value> = Vec::new();
    for message in messages {
        if message.role != gertie_core::Role::Tool && !pending.is_empty() {
            wire.push(tool_images_message(std::mem::take(&mut pending)));
        }
        if message.role == gertie_core::Role::Tool {
            let images: Vec<&String> = message
                .attachments
                .iter()
                .filter(|attachment| {
                    gertie_core::data_url_parts(attachment)
                        .is_some_and(|(media_type, _)| media_type.starts_with("image/"))
                })
                .collect();
            if !images.is_empty() {
                pending.push(serde_json::json!({
                    "type": "text",
                    "text": format!(
                        "{} ({}):",
                        message.name.clone().unwrap_or_else(|| "tool".into()),
                        message.tool_call_id.clone().unwrap_or_default()
                    ),
                }));
                pending.extend(images.into_iter().map(|image| {
                    serde_json::json!({ "type": "image_url", "image_url": { "url": image } })
                }));
            }
        }
        wire.push(wire_message(message));
    }
    if !pending.is_empty() {
        wire.push(tool_images_message(pending));
    }
    wire
}

/// The user message that carries the images a run of tool results
/// returned, OpenAI's wire has no image part for a tool message.
fn tool_images_message(parts: Vec<serde_json::Value>) -> serde_json::Value {
    let mut content = vec![serde_json::json!({
        "type": "text",
        "text": "Images the tool calls above returned:",
    })];
    content.extend(parts);
    serde_json::json!({ "role": "user", "content": content })
}

fn wire_message(message: &WireMessage) -> serde_json::Value {
    {
        {
            let mut entry = serde_json::Map::new();
            entry.insert("role".into(), serde_json::json!(wire_role(message.role)));
            match message.role {
                gertie_core::Role::Tool => {
                    // The protocol answers one call id per tool message.
                    entry.insert(
                        "tool_call_id".into(),
                        serde_json::json!(message.tool_call_id.clone().unwrap_or_default()),
                    );
                    if let Some(name) = &message.name {
                        entry.insert("name".into(), serde_json::json!(name));
                    }
                    entry.insert(
                        "content".into(),
                        serde_json::json!(message.content.clone().unwrap_or_default()),
                    );
                }
                role => {
                    // Attachments ride only on user turns: the content
                    // becomes a parts array: text first, then one part
                    // per attachment, shaped by media type: images as
                    // `image_url` parts (data URLs pass through whole),
                    // PDFs as `file` parts. No attachments: the plain
                    // string the vast majority of messages are.
                    let has_images =
                        role == gertie_core::Role::User && !message.attachments.is_empty();
                    if has_images {
                        let mut parts = vec![serde_json::json!({
                            "type": "text",
                            "text": message.content.clone().unwrap_or_default(),
                        })];
                        parts.extend(message.attachments.iter().map(|attachment| {
                            match gertie_core::data_url_parts(attachment) {
                                Some((media_type, _)) if media_type.starts_with("image/") => {
                                    serde_json::json!({
                                        "type": "image_url",
                                        "image_url": { "url": attachment },
                                    })
                                }
                                // The one document format the endpoint
                                // reads natively; the name is cosmetic:
                                // the model sees the data.
                                Some(("application/pdf", _)) => serde_json::json!({
                                    "type": "file",
                                    "file": {
                                        "filename": "document.pdf",
                                        "file_data": attachment,
                                    },
                                }),
                                _ => serde_json::json!({
                                    "type": "text",
                                    "text": "[unreadable attachment]",
                                }),
                            }
                        }));
                        entry.insert("content".into(), serde_json::Value::Array(parts));
                    } else {
                        entry.insert(
                            "content".into(),
                            serde_json::json!(message.content.clone().unwrap_or_default()),
                        );
                    }
                    if role == gertie_core::Role::Assistant && !message.tool_calls.is_empty() {
                        entry.insert(
                            "tool_calls".into(),
                            serde_json::json!(
                                message
                                    .tool_calls
                                    .iter()
                                    .map(|call| serde_json::json!({
                                        "id": call.id,
                                        "type": "function",
                                        "function": {
                                            "name": call.name,
                                            "arguments": call.arguments,
                                        },
                                    }))
                                    .collect::<Vec<_>>()
                            ),
                        );
                    }
                }
            }
            serde_json::Value::Object(entry)
        }
    }
}

fn endpoint_url(endpoint: &Endpoint) -> String {
    let base = endpoint.base_url.trim_end_matches('/');
    format!("{base}/chat/completions")
}

/// Full jitter: uniform in [0, 2^(attempt-1) * base). Clock-derived
/// randomness keeps this dependency-free; precision of the distribution
/// does not matter for a retry delay.
pub(crate) fn backoff(attempt: u32) -> Duration {
    let max = INITIAL_BACKOFF.as_millis() as u64 * (1 << (attempt - 1).min(16));
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        % (max + 1);
    Duration::from_millis(jitter)
}

fn count(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().map(|n| n.max(0) as u64))
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared test harness: a fake HTTP server that answers one POST
    //! with raw bytes, and helpers to drain a provider stream on the
    //! current thread.

    use super::*;
    use futures_core::Stream as _;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::task::{Context as TaskContext, Poll, Waker};

    /// Drains a stream to completion on the current thread. The provider's
    /// tokio tasks run on their own runtime; this only polls the receiving
    /// end, so `Pending` here means "task still producing", not deadlock.
    pub fn drain(mut stream: BoxStream<'static, Event>) -> Vec<Event> {
        let started = std::time::Instant::now();
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);
        let mut events = Vec::new();
        loop {
            match std::pin::Pin::new(&mut stream).poll_next(&mut cx) {
                Poll::Ready(Some(event)) => events.push(event),
                Poll::Ready(None) => return events,
                Poll::Pending => {
                    // The hang guard: a task wedged mid-await must fail
                    // the test fast and by name, never spin politely
                    // forever.
                    if started.elapsed() > Duration::from_secs(10) {
                        panic!("the stream stayed Pending for 10s, an await never resolved");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    /// A fake OpenAI-compatible server: responds to one POST with raw
    /// bytes (already-shaped SSE, or an error status), then closes.
    pub struct FakeServer {
        pub address: String,
    }

    impl FakeServer {
        pub fn start(response: &str) -> Self {
            Self::with_status("200 OK", response)
        }

        pub fn with_status(status_line: &str, body: &str) -> Self {
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

        pub fn endpoint(&self, wire: Wire) -> Endpoint {
            Endpoint {
                name: "fake".into(),
                base_url: format!("http://{}", self.address),
                model: "fake-model".into(),
                api_key: Some("fake-key".into()),
                context_window: None,
                vision: true,
                wire,
            }
        }
    }

    pub fn user_request() -> Request {
        Request {
            messages: vec![WireMessage::user("hi")],
            tools: Vec::new(),
            max_output_tokens: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{FakeServer, drain, user_request};
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    #[test]
    fn the_debug_of_an_endpoint_never_carries_the_key() {
        let endpoint = Endpoint {
            name: "the leak test".into(),
            base_url: "https://api.example.test".into(),
            model: "model-x".into(),
            api_key: Some("sk-super-secret-value-12345".into()),
            context_window: None,
            vision: true,
            wire: Wire::OpenAI,
        };
        let rendered = format!("{endpoint:?}");
        assert!(
            !rendered.contains("sk-super-secret-value-12345"),
            "Debug printed the key: {rendered}"
        );
        // The redaction is a presence marker, not an omission: the
        // rest of the endpoint still reads as an endpoint.
        assert!(rendered.contains("…"));
        assert!(rendered.contains("model-x"));
    }

    #[test]
    fn wire_translation_expresses_the_tool_protocol() {
        let call = ToolCall {
            id: "call_9".into(),
            name: "edit_file".into(),
            arguments: "{\"path\":\"x\"}".into(),
        };
        let messages = vec![
            WireMessage::user("fix it"),
            WireMessage::assistant_with_tool_calls(Some("on it".into()), vec![call.clone()]),
            WireMessage::tool_result(&call, "done"),
        ];

        let wire = wire_messages(&messages);
        assert_eq!(wire[0]["role"], "user");
        assert_eq!(wire[1]["tool_calls"][0]["id"], "call_9");
        assert_eq!(wire[1]["tool_calls"][0]["function"]["name"], "edit_file");
        assert_eq!(wire[2]["role"], "tool");
        assert_eq!(wire[2]["tool_call_id"], "call_9");
        assert_eq!(wire[2]["name"], "edit_file");
        assert_eq!(wire[2]["content"], "done");
    }

    #[test]
    fn a_tools_images_follow_the_run_of_results_in_a_user_message() {
        let shot = ToolCall {
            id: "call_1".into(),
            name: "screenshot".into(),
            arguments: "{}".into(),
        };
        let grep = ToolCall {
            id: "call_2".into(),
            name: "grep".into(),
            arguments: "{}".into(),
        };
        let messages = vec![
            WireMessage::assistant_with_tool_calls(None, vec![shot.clone(), grep.clone()]),
            WireMessage::tool_result_with_images(
                &shot,
                "Screenshot saved to /c/shot.png.",
                vec!["data:image/png;base64,QUJD".into()],
            ),
            WireMessage::tool_result(&grep, "no matches"),
            WireMessage::assistant("the button is clipped"),
        ];
        let wire = wire_messages(&messages);

        assert_eq!(wire.len(), 5, "{wire:#?}");
        // The tool message stays text: the wire allows nothing else,
        // and both results follow their assistant message unbroken.
        assert_eq!(wire[1]["role"], "tool");
        assert_eq!(wire[1]["content"], "Screenshot saved to /c/shot.png.");
        assert_eq!(wire[2]["role"], "tool");
        // Then one user message carries the image, labeled by call.
        assert_eq!(wire[3]["role"], "user");
        let parts = wire[3]["content"].as_array().unwrap();
        assert_eq!(parts[1]["text"], "screenshot (call_1):");
        assert_eq!(parts[2]["type"], "image_url");
        assert_eq!(parts[2]["image_url"]["url"], "data:image/png;base64,QUJD");
        assert_eq!(wire[4]["role"], "assistant");
    }

    #[test]
    fn a_model_without_vision_gets_a_note_in_place_of_each_image() {
        let call = ToolCall {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        };
        let mut attached = WireMessage::user("what's this");
        attached.attachments = vec![
            "data:image/png;base64,QUJD".into(),
            "data:application/pdf;base64,JVBERi0".into(),
        ];
        let request = Request {
            messages: vec![
                attached,
                WireMessage::assistant_with_tool_calls(None, vec![call.clone()]),
                WireMessage::tool_result_with_images(
                    &call,
                    "logo.png: image, 40x30 (1 KB).",
                    vec!["data:image/png;base64,QUJD".into()],
                ),
            ],
            ..Request::default()
        };
        let mut endpoint = Endpoint {
            name: "blind".into(),
            base_url: "http://x".into(),
            model: "text-only".into(),
            api_key: None,
            context_window: None,
            wire: Wire::OpenAI,
            vision: true,
        };
        assert!(
            matches!(
                readable_by(&endpoint, &request),
                std::borrow::Cow::Borrowed(_)
            ),
            "a model with vision gets the request as it is"
        );

        endpoint.vision = false;
        let readable = readable_by(&endpoint, &request);
        let user = &readable.messages[0];
        assert_eq!(
            user.attachments,
            vec!["data:application/pdf;base64,JVBERi0".to_string()],
            "only images go, a PDF is not one"
        );
        assert_eq!(
            user.content.as_deref(),
            Some("what's this\n[1 image not sent: this model can't read images]")
        );
        let result = &readable.messages[2];
        assert!(result.attachments.is_empty());
        assert_eq!(
            result.content.as_deref(),
            Some(
                "logo.png: image, 40x30 (1 KB).\n\
                 [1 image not sent: this model can't read images]"
            ),
            "the tool's text still names the file"
        );
    }

    #[test]
    fn user_attachments_become_content_parts() {
        let mut message = WireMessage::user("what is in this picture");
        message.attachments = vec!["data:image/png;base64,QUJD".into()];
        let wire = wire_messages(&[message]);

        // Text and image ride as a parts array, text first.
        let content = &wire[0]["content"];
        assert!(content.is_array());
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what is in this picture");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,QUJD");
        // Text-only messages stay plain strings. The overwhelmingly
        // common shape must not grow a parts wrapper.
        let plain = wire_messages(&[WireMessage::user("just words")]);
        assert_eq!(plain[0]["content"], "just words");
    }

    #[test]
    fn pdfs_become_file_parts() {
        let mut message = WireMessage::user("read this paper");
        message.attachments = vec!["data:application/pdf;base64,JVBERi0".into()];
        let wire = wire_messages(&[message]);

        let content = &wire[0]["content"];
        assert_eq!(content[1]["type"], "file");
        assert_eq!(
            content[1]["file"]["file_data"],
            "data:application/pdf;base64,JVBERi0"
        );
    }

    #[test]
    fn streams_tokens_usage_and_done_from_sse() {
        let server = FakeServer::start(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"prompt_tokens_details\":{\"cached_tokens\":5}}}\n\n\
             data: [DONE]\n\n",
        );
        let provider = OpenAICompatible::new(server.endpoint(Wire::OpenAI)).unwrap();
        let events = drain(provider.stream(user_request()));

        assert!(events.contains(&Event::Token {
            delta: "Hel".into()
        }));
        assert!(events.contains(&Event::Token { delta: "lo".into() }));
        assert!(events.contains(&Event::Usage {
            input_tokens: 7,
            output_tokens: 2,
            cached_input_tokens: 5,
        }));
        assert_eq!(events.last(), Some(&Event::Done));
    }

    /// A 429 that answers every retry ends the turn Limited: its own
    /// event, not an error. The ask is "wait, then continue", so the UI
    /// must be able to tell the two apart without string-matching.
    #[test]
    fn an_exhausted_rate_limit_ends_limited_not_errored() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr").to_string();
        std::thread::spawn(move || {
            // Every attempt gets the same 429; the retry policy burns
            // out and reports Limited.
            for socket in listener.incoming().flatten() {
                let body = "rate limit exceeded";
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: text/plain\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let mut socket = socket;
                let mut buffer = [0u8; 8192];
                let _ = socket.read(&mut buffer);
                let _ = socket.write_all(response.as_bytes());
            }
        });
        let endpoint = Endpoint {
            name: "fake".into(),
            base_url: format!("http://{address}"),
            model: "fake-model".into(),
            api_key: Some("fake-key".into()),
            context_window: None,
            vision: true,
            wire: Wire::OpenAI,
        };
        let provider = OpenAICompatible::new(endpoint).unwrap();
        let events = drain(provider.stream(user_request()));

        assert!(
            events.iter().any(
                |event| matches!(event, Event::Limited { message } if message.contains("429"))
            ),
            "the exhausted limit surfaces as Event::Limited: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Done | Event::Error { .. })),
            "a limit is terminal: no Done, no Error riding along"
        );
    }

    #[test]
    fn split_tool_call_deltas_arrive_whole() {
        let server = FakeServer::start(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"rea\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"d_file\",\"arguments\":\"th\\\":\\\"x\\\"}\"}}]}}]}\n\n\
             data: [DONE]\n\n",
        );
        let provider = OpenAICompatible::new(server.endpoint(Wire::OpenAI)).unwrap();
        let events = drain(provider.stream(user_request()));

        assert_eq!(
            events,
            vec![
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
    fn auth_errors_are_terminal_in_one_message() {
        let server = FakeServer::with_status("401 Unauthorized", "{}");
        let provider = OpenAICompatible::new(server.endpoint(Wire::OpenAI)).unwrap();
        let events = drain(provider.stream(user_request()));

        // One error, not a Started + Error progression: the loop stops
        // on the first error, so anything before the last would be
        // noise the user never gets to act on.
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], Event::Error { message } if message.contains("auth rejected"))
        );
    }

    #[test]
    fn transient_errors_retry_then_report_the_exhausted_policy() {
        let server = FakeServer::with_status("503 Service Unavailable", "overloaded");
        let provider = OpenAICompatible::new(server.endpoint(Wire::OpenAI)).unwrap();
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

    #[test]
    fn a_dead_endpoint_falls_back_to_the_next() {
        let bad = FakeServer::with_status("401 Unauthorized", "{}");
        let good = FakeServer::start(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
        );
        let provider = OpenAICompatible::with_endpoints(vec![
            bad.endpoint(Wire::OpenAI),
            good.endpoint(Wire::OpenAI),
        ])
        .unwrap();
        let events = drain(provider.stream(user_request()));

        assert!(
            matches!(events[0], Event::Token { .. }),
            "content flows from the fallback"
        );
        assert_eq!(events.last(), Some(&Event::Done));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Error { .. })),
            "the dead endpoint's 401 never reaches the turn"
        );
    }

    #[test]
    fn all_endpoints_failing_reports_once() {
        let first = FakeServer::with_status("401 Unauthorized", "{}");
        let second = FakeServer::with_status("401 Unauthorized", "{}");
        let provider = OpenAICompatible::with_endpoints(vec![
            first.endpoint(Wire::OpenAI),
            second.endpoint(Wire::OpenAI),
        ])
        .unwrap();
        let events = drain(provider.stream(user_request()));

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Event::Error { message }
                if message.contains("all endpoints failed") && message.contains("auth rejected")));
    }

    #[test]
    fn once_content_flows_the_turn_is_committed() {
        // A token, then an in-stream server error: the error passes
        // through. No silent model switch after the user has seen a
        // reply already forming.
        let server = FakeServer::start(
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n\
             data: {\"error\":{\"message\":\"overloaded mid-stream\"}}\n\n",
        );
        let provider = OpenAICompatible::new(server.endpoint(Wire::OpenAI)).unwrap();
        let events = drain(provider.stream(user_request()));

        assert!(events.contains(&Event::Token {
            delta: "partial".into()
        }));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Error { message }
                    if message.contains("overloaded mid-stream")))
        );
    }
}
