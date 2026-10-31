//! MCP (Model Context Protocol) client, tools-only — stdio and
//! Streamable HTTP transports.
//!
//! **stdio**: a server is an external process gertie spawns and speaks
//! JSON-RPC to over stdin/stdout (line-delimited JSON — MCP's original
//! transport). At connect time gertie performs the `initialize`
//! handshake and lists the server's tools; each becomes an [`McpTool`]
//! in the registry.
//!
//! **Streamable HTTP** (the 2025-03-26 transport): the server is a URL.
//! Every message is a POST of one JSON-RPC body to that single
//! endpoint; the answer comes back either as `application/json` (one
//! response) or `text/event-stream` (messages wrapped in `data:`
//! lines). The `initialize` response may carry an `Mcp-Session-Id`
//! header, which gertie echoes on every later request, alongside
//! `MCP-Protocol-Version`.
//!
//! Trust posture (from the M3 research): an MCP server is third-party
//! code talking to the model. Its tool descriptions are *untrusted
//! input* — a prompt-injection surface — so they are sanitized and
//! capped before entering the registry, and every MCP call goes through
//! the permission prompt (gertie can't know whether a foreign tool
//! mutates; the user is the capability system).
//!
//! Boring choices for v1: requests are serialized (one at a time — MCP
//! allows concurrency, but correctness first), each stdio request waits
//! at most `RESPONSE_TIMEOUT` on a reader thread (a hung server must
//! not hang the UI), HTTP requests carry the same timeout, and stdio
//! stderr is discarded rather than interleaved into model-visible
//! output.

use crate::{Tool, ToolResult, blocking};
use serde_json::{Value, json};
use std::future::Future;
use std::io::{BufRead, BufReader, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

/// How long a request may sit unanswered before it fails. Generous:
/// real MCP tools include things like scrapers and compilers.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);

/// Wire protocol version gertie speaks. Servers negotiate down; the
/// tools-only subset used here is stable across versions.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Descriptions from servers are untrusted: strip control characters
/// (terminal-escape and prompt-injection hygiene) and cap the length.
/// Delegates to the core sanitizer so the rule lives in one place.
pub(crate) fn sanitize_description(raw: &str, cap: usize) -> String {
    gertie_core::sanitize_untrusted(raw, cap)
}

fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub struct McpClient {
    server_name: String,
    inner: Arc<std::sync::Mutex<Inner>>,
    /// stdio only — the spawned server process, killed on drop. None
    /// for HTTP (nothing to reap; the server outlives the client).
    child: Option<Arc<std::sync::Mutex<std::process::Child>>>,
}

struct Inner {
    transport: Transport,
    next_id: u64,
}

enum Transport {
    Stdio {
        stdin: std::process::ChildStdin,
        /// Lines from the reader thread.
        responses: Receiver<String>,
    },
    Http {
        url: String,
        client: reqwest::blocking::Client,
        /// Echoed from the initialize response's `Mcp-Session-Id`
        /// header, when the server issues one.
        session_id: Option<String>,
    },
}

/// A server's tool, bridged into gertie's Tool trait.
pub struct McpTool {
    client: Arc<McpClient>,
    /// Name as the model calls it: `mcp_{server}_{tool}`. Leaked once at
    /// construction: `Tool::name` is `&'static str` by contract, and one
    /// allocation per configured tool is bounded by config.
    name: &'static str,
    /// The server's own name for `tools/call`.
    remote_name: String,
    description: String,
    parameters: Value,
}

impl McpClient {
    /// Spawns `command args…`, performs the initialize handshake, and
    /// returns the client. Fails cleanly if the server won't start or
    /// won't speak the protocol.
    pub fn connect(
        server_name: &str,
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self, String> {
        let mut cmd = std::process::Command::new(command);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        for (key, value) in env {
            cmd.env(key, value);
        }
        let mut child = cmd
            .spawn()
            .map_err(|error| format!("mcp `{server_name}`: could not spawn {command}: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("mcp `{server_name}`: no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("mcp `{server_name}`: no stdout"))?;

        // Reader thread: the child owns the pipe's read side; requests
        // wait on the channel with a timeout instead of blocking forever.
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name(format!("mcp-{server_name}-reader"))
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => {
                            // EOF or read error: nothing more will come.
                            // The next recv times out and reports it.
                            break;
                        }
                        Ok(_) => {
                            if tx.send(line.trim_end().to_string()).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|error| format!("mcp `{server_name}`: {error}"))?;

        let client = Self {
            server_name: server_name.to_string(),
            inner: Arc::new(std::sync::Mutex::new(Inner {
                transport: Transport::Stdio {
                    stdin,
                    responses: rx,
                },
                next_id: 1,
            })),
            child: Some(Arc::new(std::sync::Mutex::new(child))),
        };

        client.request(
            "initialize",
            Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "gertie", "version": env!("CARGO_PKG_VERSION")}
            })),
        )?;
        // Notification: no id, no response expected.
        client.notify("notifications/initialized")?;
        Ok(client)
    }

    /// Connects to a Streamable HTTP server at `url` (the MCP endpoint,
    /// not the server's root). Same handshake, no child process.
    pub fn connect_url(server_name: &str, url: &str) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(RESPONSE_TIMEOUT)
            .build()
            .map_err(|error| format!("mcp `{server_name}`: {error}"))?;
        let this = Self {
            server_name: server_name.to_string(),
            inner: Arc::new(std::sync::Mutex::new(Inner {
                transport: Transport::Http {
                    url: url.trim_end_matches('/').to_string(),
                    client,
                    session_id: None,
                },
                next_id: 1,
            })),
            child: None,
        };
        this.request(
            "initialize",
            Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "gertie", "version": env!("CARGO_PKG_VERSION")}
            })),
        )?;
        this.notify("notifications/initialized")?;
        Ok(this)
    }

    /// The server's advertised tools, as bridgable gertie tools.
    pub fn list_tools(self: &Arc<Self>) -> Result<Vec<McpTool>, String> {
        let result = self.request("tools/list", None)?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(tools
            .iter()
            .filter_map(|tool| {
                let remote_name = tool.get("name")?.as_str()?.to_string();
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                Some(McpTool {
                    client: self.clone(),
                    name: Box::leak(
                        format!(
                            "mcp_{}_{}",
                            sanitize_name(&self.server_name),
                            sanitize_name(&remote_name)
                        )
                        .into_boxed_str(),
                    ),
                    remote_name,
                    // Cap hard: untrusted text competing for context.
                    description: sanitize_description(description, 2000),
                    parameters: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or(json!({"type": "object"})),
                })
            })
            .collect())
    }

    fn notify(&self, method: &str) -> Result<(), String> {
        let notification = json!({"jsonrpc": "2.0", "method": method});
        let mut inner = self.inner.lock().unwrap();
        match &mut inner.transport {
            Transport::Stdio { stdin, .. } => writeln!(stdin, "{notification}")
                .map_err(|error| format!("mcp `{}`: {error}", self.server_name)),
            Transport::Http {
                url,
                client,
                session_id,
            } => {
                // A notification has no id and expects no response —
                // 202 Accepted or an empty body both count.
                let mut request = client
                    .post(url.as_str())
                    .header("Accept", "application/json, text/event-stream")
                    .header("MCP-Protocol-Version", PROTOCOL_VERSION)
                    .json(&notification);
                if let Some(id) = &session_id {
                    request = request.header("Mcp-Session-Id", id);
                }
                request
                    .send()
                    .map(drop)
                    .map_err(|error| format!("mcp `{}`: {error}", self.server_name))
            }
        }
    }

    fn request(&self, method: &str, params: Option<Value>) -> Result<Value, String> {
        let id = {
            let mut inner = self.inner.lock().unwrap();
            let id = inner.next_id;
            inner.next_id += 1;
            id
        };
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        // The lock guards the send and session-id state only — the
        // stdio wait re-locks per line, so it must run lock-free. The
        // HTTP round trip is self-contained (no re-lock), so it can
        // finish under the guard; requests are serialized either way.
        enum Pending {
            Stdio,
            Done(Result<Value, String>),
        }
        let sent = {
            let mut inner = self.inner.lock().unwrap();
            match &mut inner.transport {
                Transport::Stdio { stdin, .. } => {
                    writeln!(stdin, "{message}")
                        .map_err(|error| format!("mcp `{}`: {error}", self.server_name))?;
                    Pending::Stdio
                }
                Transport::Http {
                    url,
                    client,
                    session_id,
                } => {
                    let (result, issued) = Self::http_round_trip(
                        &self.server_name,
                        url,
                        client,
                        session_id.as_deref(),
                        id,
                        &message,
                    );
                    // The initialize response may issue a session id;
                    // every later request echoes it.
                    if issued.is_some() {
                        *session_id = issued;
                    }
                    Pending::Done(result)
                }
            }
        };
        match sent {
            Pending::Stdio => self.await_response_stdio(id),
            Pending::Done(result) => result,
        }
    }

    /// One Streamable HTTP round trip: POST the message, read the
    /// answer as JSON or as an SSE stream, and report any session id
    /// the server issued. Lock-free by contract (no re-lock inside).
    fn http_round_trip(
        server_name: &str,
        url: &str,
        client: &reqwest::blocking::Client,
        session_id: Option<&str>,
        id: u64,
        message: &Value,
    ) -> (Result<Value, String>, Option<String>) {
        let mut request = client
            .post(url)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(message);
        if let Some(session) = session_id {
            request = request.header("Mcp-Session-Id", session);
        }
        let response = match request.send() {
            Ok(response) => response,
            Err(error) => return (Err(format!("mcp `{server_name}`: {error}")), None),
        };
        let issued = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|value| value.to_str().ok().map(str::to_string));
        (Self::finish_http(server_name, id, response), issued)
    }

    /// The answer side, shared by every HTTP status: success is either
    /// a JSON body or an SSE stream wrapping the same JSON-RPC
    /// response; the rest of the protocol is identical either way.
    fn finish_http(
        server_name: &str,
        id: u64,
        response: reqwest::blocking::Response,
    ) -> Result<Value, String> {
        let status = response.status();
        if !status.is_success() {
            return Err(format!("mcp `{server_name}`: {status}"));
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = response
            .text()
            .map_err(|error| format!("mcp `{server_name}`: {error}"))?;
        let payload = if content_type.contains("text/event-stream") {
            body.lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .find_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
                .ok_or_else(|| format!("mcp `{server_name}`: no response in SSE stream"))?
        } else {
            serde_json::from_str::<Value>(&body)
                .map_err(|error| format!("mcp `{server_name}`: not JSON-RPC: {error}"))?
        };
        Self::rpc_result(server_name, id, &payload)
    }

    /// Shared by both transports: a parsed JSON-RPC response object is
    /// either a `result` or an `error`.
    fn rpc_result(server_name: &str, id: u64, value: &Value) -> Result<Value, String> {
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(format!(
                "mcp `{server_name}`: response id mismatch (wanted {id})"
            ));
        }
        if let Some(error) = value.get("error") {
            return Err(format!(
                "mcp `{server_name}`: {}",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("protocol error")
            ));
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    /// stdio: reads buffered lines until the response with `id`
    /// arrives. Server notifications and stale lines are skipped;
    /// timeout or EOF is an error carrying the server's last words if
    /// any.
    fn await_response_stdio(&self, id: u64) -> Result<Value, String> {
        loop {
            let line = {
                let mut inner = self.inner.lock().unwrap();
                let Transport::Stdio { responses, .. } = &mut inner.transport else {
                    unreachable!("await_response_stdio outside the stdio transport");
                };
                match responses.recv_timeout(RESPONSE_TIMEOUT) {
                    Ok(line) => line,
                    Err(RecvTimeoutError::Timeout) => {
                        return Err(format!(
                            "mcp `{}`: no response within {}s",
                            self.server_name,
                            RESPONSE_TIMEOUT.as_secs()
                        ));
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(format!(
                            "mcp `{}`: server closed its output",
                            self.server_name
                        ));
                    }
                }
            };
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => continue, // not JSON — skip (server noise)
            };
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return Self::rpc_result(&self.server_name, id, &value);
            }
            // Any other line (notifications, responses to nothing we're
            // waiting on) is dropped: model-visible output stays clean.
        }
    }
}
impl Drop for McpClient {
    fn drop(&mut self) {
        if let Some(child) = &self.child {
            let _ = child.lock().unwrap().kill();
        }
    }
}

impl Tool for McpTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> String {
        format!("[mcp] {}", self.description)
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    // Third-party code: gertie cannot know whether it mutates, so every
    // call prompts. The user is the capability system — and the mode
    // machinery honours this too: accept-edits' pre-approval stops at
    // gertie's own tools (see `third_party`).
    fn mutates(&self) -> bool {
        true
    }

    fn third_party(&self) -> bool {
        true
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        blocking(move || {
            // Arguments arrive as a JSON string; tools/call wants an
            // object. Unparseable arguments are the model's mistake —
            // reported back, not a panic.
            let parsed: Value = serde_json::from_str(arguments)
                .map_err(|error| format!("bad arguments for {}: {error}", self.remote_name))?;
            let arguments = if parsed.is_object() {
                parsed
            } else {
                json!({"value": parsed})
            };
            // request() is private; go through the same lock-serialized
            // path by calling it on the client.
            self.client
                .request(
                    "tools/call",
                    Some(json!({"name": self.remote_name, "arguments": arguments})),
                )
                .and_then(|result| {
                    let text = result
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    let is_error = result
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if is_error {
                        // A tool-level failure is model-visible, same
                        // channel as gertie's own tool errors.
                        Err(if text.is_empty() {
                            "mcp tool reported an error".to_string()
                        } else {
                            text
                        })
                    } else if text.is_empty() {
                        Ok("(no output)".to_string())
                    } else {
                        Ok(text)
                    }
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal MCP server in sh: answers initialize, tools/list, and
    /// tools/call in request order with fixed ids (matching our client's
    /// 1, 2, 3 sequence). Real framing, real child process — the parts
    /// worth testing are exactly the parts a mock object wouldn't cover.
    const FAKE_SERVER: &str = r#"
        while IFS= read -r line; do
            case "$line" in
                *'"method":"initialize"'*)
                    printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}}';;
                *'"method":"tools/list"'*)
                    printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"Echoes its input","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}},"required":["text"]}]}}';;
                *'"method":"tools/call"'*)
                    printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"echoed: hi"}]}}';;
            esac
        done
    "#;

    fn connect_fake() -> Arc<McpClient> {
        Arc::new(
            McpClient::connect("fake", "sh", &["-c".into(), FAKE_SERVER.into()], &[])
                .expect("handshake"),
        )
    }

    #[test]
    fn handshake_and_tools_list_work() {
        let client = connect_fake();
        let tools = client.list_tools().expect("tools/list");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "mcp_fake_echo");
        assert_eq!(tools[0].description(), "[mcp] Echoes its input");
        assert!(tools[0].parameters().get("properties").is_some());
        // Third-party: prompts unconditionally.
        assert!(tools[0].mutates());
    }

    #[test]
    fn tool_calls_round_trip() {
        let client = connect_fake();
        let tools = client.list_tools().expect("tools/list");
        let result = futures_now(tools[0].run(r#"{"text": "hi"}"#));
        assert_eq!(result.unwrap(), "echoed: hi");
    }

    /// Tests run outside a runtime; our tools' futures are already
    /// computed by `blocking`, so a plain poll loop suffices.
    fn futures_now(
        mut future: Pin<Box<dyn Future<Output = ToolResult> + Send + '_>>,
    ) -> ToolResult {
        use std::task::{Context, Poll, Waker};
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            match Pin::new(&mut future).poll(&mut cx) {
                Poll::Ready(result) => return result,
                Poll::Pending => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    #[test]
    fn descriptions_are_sanitized_and_capped() {
        // Control characters stripped (injection hygiene), length capped.
        let dirty = format!("legit text\u{1b}[2J\u{7}more{0}", "x".repeat(3000));
        let clean = sanitize_description(&dirty, 2000);
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.contains('\u{7}'));
        assert_eq!(clean.chars().count(), 2000);
    }

    #[test]
    fn names_keep_to_the_registry_charset() {
        assert_eq!(sanitize_name("my-tool.v2"), "my_tool_v2");
        assert_eq!(sanitize_name("plain"), "plain");
    }

    /// A fake Streamable HTTP MCP server: accepts repeated connections,
    /// answers initialize / notifications/initialized / tools/list /
    /// tools/call in request order, records the `Mcp-Session-Id` header
    /// of every request. Real HTTP over a real socket — the parts worth
    /// testing (framing, session echo, SSE wrapping) are exactly the
    /// parts a mock wouldn't cover.
    struct FakeHttpServer {
        url: String,
        seen_sessions: Arc<std::sync::Mutex<Vec<Option<String>>>>,
    }

    impl FakeHttpServer {
        fn start(list_content_type: &'static str) -> Self {
            Self::start_with(list_content_type, false)
        }

        fn start_with(list_content_type: &'static str, require_session: bool) -> Self {
            use std::io::Read as _;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let address = listener.local_addr().expect("addr").to_string();
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen_for_thread = seen.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let mut stream = match stream {
                        Ok(stream) => stream,
                        Err(_) => break,
                    };
                    // Headers, then exactly content-length more bytes.
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                        let header_end = buffer
                            .windows(4)
                            .position(|window| window == b"\r\n\r\n")
                            .map(|position| position + 4);
                        let Some(header_end) = header_end else {
                            continue;
                        };
                        let content_length = String::from_utf8_lossy(&buffer[..header_end])
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        while buffer.len() < header_end + content_length {
                            match stream.read(&mut chunk) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                            }
                        }
                        break;
                    }
                    let raw = String::from_utf8_lossy(&buffer).to_string();
                    let (headers, body) = match raw.split_once("\r\n\r\n") {
                        Some(parts) => parts,
                        None => continue,
                    };
                    seen_for_thread.lock().unwrap().push(
                        headers
                            .lines()
                            .find(|line| line.to_ascii_lowercase().starts_with("mcp-session-id:"))
                            .map(str::to_string),
                    );
                    if require_session && body.contains("\"method\":\"tools/list\"") {
                        let has_session = headers
                            .lines()
                            .any(|line| line.eq_ignore_ascii_case("mcp-session-id: sess-42"));
                        if !has_session {
                            let response = "HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                            stream.write_all(response.as_bytes()).unwrap();
                            continue;
                        }
                    }
                    let (content_type, body) = if body.contains("\"method\":\"initialize\"") {
                        (
                            "application/json",
                            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}}"#,
                        )
                    } else if body.contains("notifications/initialized") {
                        // A notification: acknowledged, no body.
                        // `connection: close` matters — without it the
                        // client may pool the socket this test server
                        // is about to drop, and the next request dies
                        // with a reset (the flake).
                        stream
                            .write_all(
                                b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                            )
                            .unwrap();
                        continue;
                    } else if body.contains("\"method\":\"tools/list\"") {
                        (
                            list_content_type,
                            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"Echoes its input","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}},"required":["text"]}]}}"#,
                        )
                    } else if body.contains("\"method\":\"tools/call\"") {
                        (
                            "application/json",
                            r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"echoed: hi"}]}}"#,
                        )
                    } else {
                        (
                            "application/json",
                            r#"{"jsonrpc":"2.0","id":99,"error":{"code":-32601,"message":"method not found"}}"#,
                        )
                    };
                    let payload = if content_type.contains("text/event-stream") {
                        format!("data: {body}\n\n")
                    } else {
                        body.to_string()
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\n\
                         mcp-session-id: sess-42\r\ncontent-length: {}\r\n\
                         connection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    stream.write_all(response.as_bytes()).unwrap();
                }
            });
            Self {
                url: format!("http://{address}/mcp"),
                seen_sessions: seen,
            }
        }
    }

    #[test]
    fn http_handshake_tools_and_calls_round_trip() {
        let server = FakeHttpServer::start("application/json");
        let client = Arc::new(McpClient::connect_url("fake", &server.url).expect("handshake"));
        let tools = client.list_tools().expect("tools/list");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "mcp_fake_echo");
        let result = futures_now(tools[0].run(r#"{"text": "hi"}"#));
        assert_eq!(result.unwrap(), "echoed: hi");

        // Session discipline: initialize carries no id, everything
        // after echoes the one the server issued.
        let seen = server.seen_sessions.lock().unwrap();
        assert_eq!(seen.len(), 4, "initialize, notification, list, call");
        assert_eq!(seen[0], None, "initialize has no session yet");
        assert!(
            seen[1..]
                .iter()
                .all(|header| header.as_deref() == Some("mcp-session-id: sess-42")),
            "every later request echoes the session id: {seen:?}"
        );
    }

    #[test]
    fn http_server_refusing_sessionless_requests_is_survived() {
        // A strict server (drops sessionless tools/list) proves gertie
        // echoes the session id — the handshake succeeds only because
        // of the echo.
        let server = FakeHttpServer::start_with("application/json", true);
        let client = Arc::new(McpClient::connect_url("fake", &server.url).expect("handshake"));
        let tools = client.list_tools().expect("tools/list");
        assert_eq!(tools.len(), 1);
    }

    #[test]
    fn http_sse_wrapped_responses_parse() {
        // Some servers answer every POST with text/event-stream; the
        // client unwraps the data: line and carries on.
        let server = FakeHttpServer::start("text/event-stream");
        let client = Arc::new(McpClient::connect_url("fake", &server.url).expect("handshake"));
        let tools = client.list_tools().expect("tools/list");
        assert_eq!(tools[0].name(), "mcp_fake_echo");
    }
}
