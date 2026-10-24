//! MCP (Model Context Protocol) stdio client, tools-only.
//!
//! A server is an external process gertie spawns and speaks JSON-RPC to
//! over stdin/stdout (line-delimited JSON — MCP's stdio transport). At
//! connect time gertie performs the `initialize` handshake and lists the
//! server's tools; each becomes an [`McpTool`] in the registry.
//!
//! Trust posture (from the M3 research): an MCP server is third-party
//! code talking to the model. Its tool descriptions are *untrusted
//! input* — a prompt-injection surface — so they are sanitized and
//! capped before entering the registry, and every MCP call goes through
//! the permission prompt (gertie can't know whether a foreign tool
//! mutates; the user is the capability system).
//!
//! Boring choices for v1: requests are serialized (one at a time — MCP
//! allows concurrency, but correctness first), each request waits at
//! most `RESPONSE_TIMEOUT` on a reader thread (a hung server must not
//! hang the UI), and stderr is discarded rather than interleaved into
//! model-visible output.

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
fn sanitize_description(raw: &str, cap: usize) -> String {
    let filtered: String = raw
        .chars()
        .filter(|c| !(c.is_control() && !c.is_whitespace()))
        .collect();
    filtered.chars().take(cap).collect()
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
    child: Arc<std::sync::Mutex<std::process::Child>>,
}

struct Inner {
    stdin: std::process::ChildStdin,
    responses: Receiver<String>,
    next_id: u64,
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
                stdin,
                responses: rx,
                next_id: 1,
            })),
            child: Arc::new(std::sync::Mutex::new(child)),
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
        let line = json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        let mut inner = self.inner.lock().unwrap();
        writeln!(inner.stdin, "{line}")
            .map_err(|error| format!("mcp `{}`: {error}", self.server_name))
    }

    fn request(&self, method: &str, params: Option<Value>) -> Result<Value, String> {
        let (id, line) = {
            let mut inner = self.inner.lock().unwrap();
            let id = inner.next_id;
            inner.next_id += 1;
            let mut line = json!({"jsonrpc": "2.0", "id": id, "method": method});
            if let Some(params) = params {
                line["params"] = params;
            }
            (id, line)
        };
        {
            let mut inner = self.inner.lock().unwrap();
            writeln!(inner.stdin, "{line}")
                .map_err(|error| format!("mcp `{}`: {error}", self.server_name))?;
        }
        self.await_response(id)
    }

    /// Reads buffered lines until the response with `id` arrives. Server
    /// notifications and stale lines are skipped; timeout or EOF is an
    /// error carrying the server's last words if any.
    fn await_response(&self, id: u64) -> Result<Value, String> {
        loop {
            let line = {
                let inner = self.inner.lock().unwrap();
                match inner.responses.recv_timeout(RESPONSE_TIMEOUT) {
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
                if let Some(error) = value.get("error") {
                    return Err(format!(
                        "mcp `{}`: {}",
                        self.server_name,
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("protocol error")
                    ));
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            // Any other line (notifications, responses to nothing we're
            // waiting on) is dropped: model-visible output stays clean.
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.lock().unwrap().kill();
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
    // call prompts. The user is the capability system.
    fn mutates(&self) -> bool {
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
}
