//! Anthropic Messages API adapter, the second wire, and the honest
//! test of whether the [`Provider`] trait is vendor-neutral. Everything
//! vendor-specific dies here: the request body (system is top-level,
//! `max_tokens` is required, tool results ride user messages), the SSE
//! event shapes (content blocks with `input_json_delta` fragments), and
//! the auth header (`x-api-key`, not a bearer token).
//!
//! The retry + fallback envelope is the same shape as the OpenAI
//! adapter's: silent retries, fallback across endpoints until one
//! produces content, commit on first content. Two adapters share the
//! shape; a third would justify extracting it.

use crate::{Endpoint, Outcome, RequestScoped, Terminal};
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use gertie_core::Event;
use gertie_core::request::Request;
use tokio_stream::wrappers::UnboundedReceiverStream;

/// Anthropic requires max_tokens on every request; the loop fills it
/// when it knows better, this is the floor.
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 8192;
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct Anthropic {
    endpoints: Vec<Endpoint>,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl Anthropic {
    pub fn context_window(&self) -> Option<u64> {
        self.endpoints
            .first()
            .and_then(|endpoint| endpoint.context_window)
    }
}

impl Anthropic {
    pub fn new(endpoint: Endpoint) -> Result<Self, reqwest::Error> {
        Self::with_endpoints(vec![endpoint])
    }

    pub fn with_endpoints(endpoints: Vec<Endpoint>) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder().build()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        Ok(Self {
            endpoints,
            client,
            runtime,
        })
    }
}

/// One partially accumulated tool call: Anthropic streams a tool use as
/// a `tool_use` block start plus `input_json_delta` fragments.
#[derive(Default)]
struct PartialToolUse {
    id: String,
    name: String,
    arguments: String,
}

/// The canonical conversation in Anthropic's shape: system text pulled
/// out top-level, tool results expressed as user-message blocks.
pub(crate) fn wire_messages(request: &Request) -> (Option<String>, serde_json::Value) {
    let mut system: Vec<String> = Vec::new();
    let mut messages: Vec<serde_json::Value> = Vec::new();
    for message in &request.messages {
        match message.role {
            gertie_core::Role::System => {
                if let Some(content) = &message.content {
                    system.push(content.clone());
                }
            }
            gertie_core::Role::User => {
                // Attachments ride as typed blocks, text first,
                // Anthropic's multimodal order. Images go as image
                // blocks, PDFs as document blocks. A data URL that
                // doesn't parse (shouldn't happen; the UI builds them)
                // degrades to a text note rather than a 400 from the
                // endpoint.
                let mut content = vec![serde_json::json!({
                    "type": "text",
                    "text": message.content.clone().unwrap_or_default(),
                })];
                for attachment in &message.attachments {
                    match gertie_core::data_url_parts(attachment) {
                        Some((media_type, data)) if media_type.starts_with("image/") => content
                            .push(serde_json::json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": media_type,
                                    "data": data,
                                },
                            })),
                        Some(("application/pdf", data)) => content.push(serde_json::json!({
                            "type": "document",
                            "source": {
                                "type": "base64",
                                "media_type": "application/pdf",
                                "data": data,
                            },
                        })),
                        Some((media_type, _)) => content.push(serde_json::json!({
                            "type": "text",
                            "text": format!("[attachment the endpoint can't read: {media_type}]"),
                        })),
                        None => content.push(serde_json::json!({
                            "type": "text",
                            "text": "[unreadable attachment]",
                        })),
                    }
                }
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": content,
                }));
            }
            gertie_core::Role::Assistant => {
                let mut content: Vec<serde_json::Value> = Vec::new();
                if let Some(text) = &message.content
                    && !text.is_empty()
                {
                    content.push(serde_json::json!({ "type": "text", "text": text }));
                }
                for call in &message.tool_calls {
                    // `input` must be an object; an unparseable arguments
                    // string is the model's mistake. Hand it through as
                    // an empty one rather than inventing content.
                    let input = serde_json::from_str::<serde_json::Value>(&call.arguments)
                        .ok()
                        .unwrap_or(serde_json::json!({}));
                    content.push(serde_json::json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": input,
                    }));
                }
                messages.push(serde_json::json!({ "role": "assistant", "content": content }));
            }
            gertie_core::Role::Tool => {
                // Anthropic speaks results as user messages.
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                        "content": message.content.clone().unwrap_or_default(),
                    }],
                }));
            }
        }
    }
    let system = if system.is_empty() {
        None
    } else {
        Some(system.join("\n\n"))
    };
    (system, serde_json::json!(messages))
}

fn endpoint_url(endpoint: &Endpoint) -> String {
    let base = endpoint.base_url.trim_end_matches('/');
    format!("{base}/v1/messages")
}

impl Anthropic {
    async fn attempt_once(
        scoped: &RequestScoped,
        request: &Request,
        sender: &tokio::sync::mpsc::UnboundedSender<Event>,
    ) -> Outcome {
        let (system, messages) = wire_messages(request);
        let mut body = serde_json::json!({
            "model": scoped.endpoint.model,
            "max_tokens": request
                .max_output_tokens
                .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
            "stream": true,
            "messages": messages,
        });
        if let Some(system) = system {
            body["system"] = serde_json::json!(system);
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::json!(
                request
                    .tools
                    .iter()
                    .map(|spec| serde_json::json!({
                        "name": spec.name,
                        "description": spec.description,
                        "input_schema": spec.parameters,
                    }))
                    .collect::<Vec<_>>()
            );
        }

        let mut request = scoped
            .client
            .post(endpoint_url(&scoped.endpoint))
            .json(&body);
        if let Some(key) = &scoped.endpoint.api_key {
            request = request.header("x-api-key", key);
        }
        request = request.header("anthropic-version", ANTHROPIC_VERSION);

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => return Outcome::Transient(format!("connection failed: {error}")),
        };

        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Outcome::Fatal(format!("auth rejected ({status}): check the API key"));
        }
        // 429 is the rate limit: own outcome, terminal as Limited. 529
        // is Anthropic's overloaded signal (a server blip), retried as
        // a transient.
        if status.as_u16() == 429 {
            let detail = response.text().await.unwrap_or_default();
            return Outcome::Limited(format!("{status}: {}", crate::truncate(&detail, 200)));
        }
        if status.is_server_error() || status.as_u16() == 529 {
            let detail = response.text().await.unwrap_or_default();
            return Outcome::Transient(format!("{status}: {}", crate::truncate(&detail, 200)));
        }
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Outcome::Fatal(format!("{status}: {}", crate::truncate(&detail, 200)));
        }

        parse_sse(response.bytes_stream(), sender).await
    }
}

impl gertie_core::Provider for Anthropic {
    fn stream(&self, request: Request) -> BoxStream<'static, Event> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let endpoints = self.endpoints.clone();
        let client = self.client.clone();
        self.runtime.spawn(async move {
            let mut last_error: Option<String> = None;
            for endpoint in endpoints {
                let scoped = RequestScoped {
                    endpoint,
                    client: client.clone(),
                };
                let (proxy, mut proxy_rx) = tokio::sync::mpsc::unbounded_channel();
                let request = request.clone();
                let driver = tokio::spawn(async move {
                    // One endpoint's full retry policy: silent retries
                    // (an Event::Error is terminal for the turn), the
                    // same contract as the OpenAI envelope. The proxy
                    // sender is owned here so its drop marks the end of
                    // the stream for the forwarder.
                    let mut attempt = 0;
                    loop {
                        attempt += 1;
                        let _ = proxy.send(Event::Started);
                        match Anthropic::attempt_once(&scoped, &request, &proxy).await {
                            Outcome::Done => break Ok(()),
                            Outcome::Fatal(message) => break Err(Terminal::Error(message)),
                            Outcome::Transient(message) => {
                                if attempt >= crate::MAX_ATTEMPTS {
                                    break Err(Terminal::Error(format!(
                                        "{message} (after {attempt} attempts)"
                                    )));
                                }
                                tokio::time::sleep(crate::backoff(attempt)).await;
                            }
                            Outcome::Limited(message) => {
                                if attempt >= crate::MAX_ATTEMPTS {
                                    break Err(Terminal::Limited(format!(
                                        "{message} (after {attempt} attempts)"
                                    )));
                                }
                                tokio::time::sleep(crate::backoff(attempt)).await;
                            }
                        }
                    }
                });
                // Forward live, but hold everything back until this
                // endpoint has produced content (same contract as the
                // OpenAI envelope).
                let mut produced = false;
                while let Some(event) = proxy_rx.recv().await {
                    match &event {
                        Event::Token { .. } | Event::ToolCall { .. } | Event::Usage { .. } => {
                            produced = true
                        }
                        _ if !produced => continue,
                        _ => {}
                    }
                    let _ = sender.send(event);
                }
                if produced {
                    return;
                }
                let outcome = driver
                    .await
                    .unwrap_or_else(|_| Err(Terminal::Error("stream driver failed".into())));
                match outcome {
                    Ok(()) => {
                        let _ = sender.send(Event::Done);
                        return;
                    }
                    Err(Terminal::Error(message)) => last_error = Some(message),
                    // A limit isn't an endpoint failure to fall back
                    // from. Other endpoints share the same account.
                    Err(Terminal::Limited(message)) => {
                        let _ = sender.send(Event::Limited {
                            message: format!("rate limited: {message}"),
                        });
                        return;
                    }
                }
            }
            let detail = last_error.unwrap_or_else(|| "no endpoints configured".into());
            let _ = sender.send(Event::Error {
                message: format!("all endpoints failed: {detail}"),
            });
        });
        Box::pin(UnboundedReceiverStream::new(receiver))
    }
}

/// Anthropic's SSE: the event type is in the data payload's `type`
/// field (the `event:` line is redundant), so one line-oriented `data:`
/// parser dispatches on it. Tool calls arrive as a `tool_use` block
/// start plus `input_json_delta` fragments; gertie normalizes them to
/// whole calls, same contract as the OpenAI path.
async fn parse_sse(
    stream: impl futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>>,
    sender: &tokio::sync::mpsc::UnboundedSender<Event>,
) -> Outcome {
    let mut stream = std::pin::pin!(stream);
    let mut buffer: Vec<u8> = Vec::new();
    let mut partials: Vec<(usize, PartialToolUse)> = Vec::new();
    let mut input_tokens: Option<u64> = None;
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
            let Ok(value) = serde_json::from_str::<serde_json::Value>(payload.trim()) else {
                continue;
            };
            match value.get("type").and_then(|kind| kind.as_str()) {
                Some("message_start") => {
                    input_tokens = value
                        .pointer("/message/usage/input_tokens")
                        .and_then(|count| count.as_u64());
                }
                Some("content_block_start") => {
                    let index = value
                        .get("index")
                        .and_then(|index| index.as_u64())
                        .unwrap_or(0) as usize;
                    let block = value.pointer("/content_block").cloned().unwrap_or_default();
                    if block.get("type").and_then(|kind| kind.as_str()) == Some("tool_use") {
                        partials.push((
                            index,
                            PartialToolUse {
                                id: block
                                    .get("id")
                                    .and_then(|id| id.as_str())
                                    .unwrap_or_default()
                                    .into(),
                                name: block
                                    .get("name")
                                    .and_then(|name| name.as_str())
                                    .unwrap_or_default()
                                    .into(),
                                arguments: String::new(),
                            },
                        ));
                    }
                }
                Some("content_block_delta") => {
                    let index = value
                        .get("index")
                        .and_then(|index| index.as_u64())
                        .unwrap_or(0) as usize;
                    let delta = value.get("delta").cloned().unwrap_or_default();
                    match delta.get("type").and_then(|kind| kind.as_str()) {
                        Some("text_delta") => {
                            if let Some(text) = delta.get("text").and_then(|text| text.as_str()) {
                                let _ = sender.send(Event::Token { delta: text.into() });
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(fragment) =
                                delta.get("partial_json").and_then(|json| json.as_str())
                                && let Some(partial) =
                                    partials.iter_mut().find(|(block, _)| *block == index)
                            {
                                partial.1.arguments.push_str(fragment);
                            }
                        }
                        _ => {}
                    }
                }
                Some("content_block_stop") => {
                    let index = value
                        .get("index")
                        .and_then(|index| index.as_u64())
                        .unwrap_or(0) as usize;
                    if let Some(position) = partials.iter().position(|(block, _)| *block == index) {
                        let partial = partials.remove(position).1;
                        let _ = sender.send(Event::ToolCall {
                            id: partial.id,
                            name: partial.name,
                            arguments: partial.arguments,
                        });
                    }
                }
                Some("message_delta") => {
                    let output_tokens = value
                        .pointer("/usage/output_tokens")
                        .and_then(|count| count.as_u64())
                        .unwrap_or(0);
                    let _ = sender.send(Event::Usage {
                        input_tokens: input_tokens.unwrap_or(0),
                        output_tokens,
                    });
                }
                Some("message_stop") => {
                    let _ = sender.send(Event::Done);
                    return Outcome::Done;
                }
                Some("error") => {
                    let message = value
                        .pointer("/error/message")
                        .and_then(|message| message.as_str())
                        .unwrap_or("unknown error")
                        .to_string();
                    let _ = sender.send(Event::Error { message });
                    return Outcome::Done;
                }
                _ => {} // ping and anything unrecognized
            }
        }
    }
    // Server closed without message_stop: flush whatever accumulated.
    for (_, partial) in partials {
        let _ = sender.send(Event::ToolCall {
            id: partial.id,
            name: partial.name,
            arguments: partial.arguments,
        });
    }
    let _ = sender.send(Event::Done);
    Outcome::Done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Wire;
    use crate::test_support::{FakeServer, drain, user_request};
    use gertie_core::Provider as _;
    use gertie_core::request::WireMessage;
    use gertie_core::{Role, ToolCall};

    #[test]
    fn user_attachments_become_base64_source_blocks() {
        let mut message = WireMessage::user("what is in this picture");
        message.attachments = vec!["data:image/jpeg;base64,QUJD".into()];
        let request = Request {
            messages: vec![message],
            ..Request::default()
        };
        let (system, wire) = wire_messages(&request);

        assert!(system.is_none());
        // Text and image ride as blocks, text first, base64 source split
        // back into media type + payload.
        let content = &wire[0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what is in this picture");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/jpeg");
        assert_eq!(content[1]["source"]["data"], "QUJD");
        // An unparseable data URL degrades to a text note, not a 400.
        let mut broken = WireMessage::user("look");
        broken.attachments = vec!["not a data url".into()];
        let request = Request {
            messages: vec![broken],
            ..Request::default()
        };
        let (_, wire) = wire_messages(&request);
        assert_eq!(wire[0]["content"][1]["type"], "text");
        assert_eq!(wire[0]["content"][1]["text"], "[unreadable attachment]");
    }

    #[test]
    fn pdfs_become_document_blocks() {
        let mut message = WireMessage::user("read this paper");
        message.attachments = vec!["data:application/pdf;base64,JVBERi0".into()];
        let request = Request {
            messages: vec![message],
            ..Request::default()
        };
        let (_, wire) = wire_messages(&request);

        assert_eq!(wire[0]["content"][1]["type"], "document");
        assert_eq!(wire[0]["content"][1]["source"]["type"], "base64");
        assert_eq!(
            wire[0]["content"][1]["source"]["media_type"],
            "application/pdf"
        );
        assert_eq!(wire[0]["content"][1]["source"]["data"], "JVBERi0");
    }

    #[test]
    fn streams_text_usage_and_done_from_sse() {
        let server = FakeServer::start(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":25}}}\n\n\
             data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n\
             data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n\
             data: {\"type\":\"message_stop\"}\n\n",
        );
        let provider = Anthropic::new(server.endpoint(Wire::Anthropic)).unwrap();
        let events = drain(provider.stream(user_request()));

        assert!(events.contains(&Event::Token {
            delta: "Hel".into()
        }));
        assert!(events.contains(&Event::Token { delta: "lo".into() }));
        assert!(events.contains(&Event::Usage {
            input_tokens: 25,
            output_tokens: 2
        }));
        assert_eq!(events.last(), Some(&Event::Done));
    }

    #[test]
    fn tool_use_blocks_arrive_whole() {
        let server = FakeServer::start(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n\
             data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}}\n\n\
             data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"pa\"}}\n\n\
             data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"th\\\":\\\"x\\\"}\"}}\n\n\
             data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":7}}\n\n\
             data: {\"type\":\"message_stop\"}\n\n",
        );
        let provider = Anthropic::new(server.endpoint(Wire::Anthropic)).unwrap();
        let events = drain(provider.stream(user_request()));

        assert_eq!(
            events,
            vec![
                Event::ToolCall {
                    id: "toolu_1".into(),
                    name: "read_file".into(),
                    arguments: "{\"path\":\"x\"}".into(),
                },
                Event::Usage {
                    input_tokens: 10,
                    output_tokens: 7
                },
                Event::Done,
            ]
        );
    }

    #[test]
    fn wire_translation_pulls_system_out_and_rides_results_on_user() {
        let call = ToolCall {
            id: "toolu_9".into(),
            name: "edit_file".into(),
            arguments: "{\"path\":\"x\"}".into(),
        };
        let request = Request {
            messages: vec![
                WireMessage {
                    role: Role::System,
                    content: Some("be terse".into()),
                    ..Default::default()
                },
                WireMessage::user("fix it"),
                WireMessage::assistant_with_tool_calls(Some("on it".into()), vec![call.clone()]),
                WireMessage::tool_result(&call, "done"),
            ],
            tools: Vec::new(),
            max_output_tokens: None,
        };
        let (system, messages) = wire_messages(&request);

        assert_eq!(
            system.as_deref(),
            Some("be terse"),
            "system is top-level, not a message"
        );
        assert_eq!(messages.as_array().unwrap().len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "text");
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[1]["content"][1]["id"], "toolu_9");
        assert_eq!(messages[1]["content"][1]["input"]["path"], "x");
        assert_eq!(
            messages[2]["role"], "user",
            "tool results ride user messages"
        );
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_9");
        assert_eq!(messages[2]["content"][0]["content"], "done");
    }

    #[test]
    fn a_dead_endpoint_falls_back_to_the_next() {
        let bad = FakeServer::with_status("401 Unauthorized", "{}");
        let good = FakeServer::start(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
             data: {\"type\":\"message_stop\"}\n\n",
        );
        let provider = Anthropic::with_endpoints(vec![
            bad.endpoint(Wire::Anthropic),
            good.endpoint(Wire::Anthropic),
        ])
        .unwrap();
        let events = drain(provider.stream(user_request()));

        assert!(events.contains(&Event::Token { delta: "hi".into() }));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Error { .. }))
        );
    }
}
