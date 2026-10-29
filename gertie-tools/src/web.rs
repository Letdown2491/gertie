//! Web tools: fetch a page as text, search the web (when configured),
//! and search gertie's own session history.
//!
//! Boring choices: fetch is reqwest-blocking with a hard timeout and
//! runs under the same blocking seam as every other tool; HTML is
//! stripped to text (no headless browser, no JS); search backends are
//! pluggable and **gracefully disabled** — an unconfigured backend is a
//! clear model-visible message, never a broken turn.

use crate::{Tool, ToolResult, blocking};
use serde_json::json;
use std::io::Read as _;
use std::sync::Arc;

/// Matches the builtin tools' cap: one result may not eat the window.
const RESULT_CHAR_LIMIT: usize = 16_000;
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const USER_AGENT: &str = concat!("gertie/", env!("CARGO_PKG_VERSION"));

/// Strips HTML to readable text: noisy elements dropped whole, tags
/// become newlines, entities decoded, whitespace collapsed. Best-effort
/// — the goal is model-readable text, not typographic fidelity.
fn html_to_text(html: &str) -> String {
    let noisy_tag = |name: &str| matches!(name, "script" | "style" | "head" | "noscript" | "svg");
    let block_tag = |name: &str| {
        matches!(
            name,
            "p" | "br" | "li" | "tr" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
        )
    };

    let mut out = String::new();
    let mut skip: Vec<&'static str> = Vec::new();
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        if skip.is_empty() {
            out.push_str(&rest[..lt]);
            out.push('\n');
        }
        let after = &rest[lt..];
        if after.starts_with("<!--") {
            match after.find("-->") {
                Some(end) => rest = &after[end + 3..],
                None => break,
            }
            continue;
        }
        match after.find('>') {
            Some(gt) => {
                let inner = &after[1..gt];
                rest = &after[gt + 1..];
                let (closing, name_part) = match inner.trim().strip_prefix('/') {
                    Some(stripped) => (true, stripped),
                    None => (false, inner.trim()),
                };
                let name = name_part
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                if noisy_tag(&name) {
                    if closing {
                        skip.pop();
                    } else if !name_part.ends_with('/') {
                        skip.push(name.leak() as &'static str);
                    }
                } else if block_tag(&name) && skip.is_empty() {
                    out.push('\n');
                }
            }
            None => break, // malformed: unterminated tag
        }
    }
    if skip.is_empty() {
        out.push_str(rest);
    }

    // Entities (the common five) + whitespace collapse.
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut final_text = String::new();
    let mut blanks = 0;
    for line in decoded.lines() {
        let line = line.trim();
        if line.is_empty() {
            blanks += 1;
            if blanks > 2 {
                continue;
            }
        } else {
            blanks = 0;
        }
        final_text.push_str(line);
        final_text.push('\n');
    }
    final_text
}

/// Fetches a URL and returns extracted text with the title.
pub struct WebFetch;

impl Tool for WebFetch {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> String {
        "Fetch a web page (http/https) and return its readable text. \
         No JavaScript is executed; pages that require it come back thin."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "absolute http(s) URL"}
            },
            "required": ["url"]
        })
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        blocking(move || {
            #[derive(serde::Deserialize)]
            struct Args {
                url: String,
            }
            let args: Args = crate::parse(arguments, "web_fetch")?;
            // Scheme gate: no file://, no ftp://, no surprises.
            if !args.url.starts_with("http://") && !args.url.starts_with("https://") {
                return Err("only http and https URLs are fetched".into());
            }

            let client = reqwest::blocking::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .user_agent(USER_AGENT)
                .build()
                .map_err(|error| format!("web_fetch: {error}"))?;
            let response = client
                .get(&args.url)
                .send()
                .map_err(|error| format!("web_fetch {url}: {error}", url = args.url))?;
            let status = response.status();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.split(';').next().unwrap_or(value).to_string())
                .unwrap_or_default();
            let mut body = String::new();
            response
                .take(8 * 1024 * 1024)
                .read_to_string(&mut body)
                .map_err(|error| format!("web_fetch: {error}"))?;

            let mut head = format!("{} {status} · {content_type}", args.url);
            if !status.is_success() {
                head.push_str(" (error)");
            }
            let text = if content_type.contains("html") {
                html_to_text(&body)
            } else if content_type.is_empty()
                || content_type.contains("text")
                || content_type.contains("json")
            {
                body
            } else {
                "(binary content not shown)".to_string()
            };

            let mut out = format!("{head}\n\n{text}");
            if out.chars().count() > RESULT_CHAR_LIMIT {
                out = out.chars().take(RESULT_CHAR_LIMIT).collect::<String>() + "\n… [truncated]";
            }
            Ok(out)
        })
    }
}

/// Web search. Configured backends: Brave (API key) and SearXNG
/// (self-hosted or public instance allowing JSON). Unconfigured, the
/// tool exists and explains itself — the model can tell the user what
/// to add rather than failing mysteriously.
pub struct WebSearch {
    backend: Option<SearchBackend>,
}

enum SearchBackend {
    Brave { api_key: String },
    Searxng { base_url: String },
}

impl WebSearch {
    pub fn unconfigured() -> Self {
        Self { backend: None }
    }

    pub fn brave(api_key: String) -> Self {
        Self {
            backend: Some(SearchBackend::Brave { api_key }),
        }
    }

    pub fn searxng(base_url: String) -> Self {
        Self {
            backend: Some(SearchBackend::Searxng { base_url }),
        }
    }
}

impl Tool for WebSearch {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> String {
        "Search the web and return result titles, URLs, and snippets. \
         Use for current information beyond your knowledge."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "the search query"},
                "count": {"type": "integer", "description": "results to return (default 5, max 10)"}
            },
            "required": ["query"]
        })
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        blocking(move || {
            #[derive(serde::Deserialize)]
            struct Args {
                query: String,
                count: Option<usize>,
            }
            let args: Args = crate::parse(arguments, "web_search")?;
            let count = args.count.unwrap_or(5).clamp(1, 10);

            let Some(backend) = &self.backend else {
                return Err(
                    "no search backend configured. Add to ~/.config/gertie/config.toml:\n\
                     \x20 [search]\n\
                     \x20 backend = \"brave\"            # or \"searxng\"\n\
                     \x20 api_key_env = \"BRAVE_API_KEY\" # for brave\n\
                     \x20 base_url = \"https://searx.example\"  # for searxng"
                        .into(),
                );
            };

            let client = reqwest::blocking::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .user_agent(USER_AGENT)
                .build()
                .map_err(|error| format!("web_search: {error}"))?;

            let mut results = String::new();
            match backend {
                SearchBackend::Brave { api_key } => {
                    let response = client
                        .get("https://api.search.brave.com/res/v1/web/search")
                        .query(&[("q", args.query.as_str()), ("count", &count.to_string())])
                        .header("X-Subscription-Token", api_key)
                        .send()
                        .map_err(|error| format!("web_search: {error}"))?;
                    let status = response.status();
                    let body: serde_json::Value = response
                        .json()
                        .map_err(|error| format!("web_search: {error}"))?;
                    if !status.is_success() {
                        return Err(format!(
                            "web_search: brave returned {status} — {}",
                            body.get("error")
                                .map(|error| error.to_string())
                                .unwrap_or_default()
                        ));
                    }
                    for (index, result) in body["web"]["results"]
                        .as_array()
                        .map(|results| results.as_slice())
                        .unwrap_or(&[])
                        .iter()
                        .take(count)
                        .enumerate()
                    {
                        results.push_str(&format!(
                            "{}. {}\n   {}\n   {}\n\n",
                            index + 1,
                            result["title"].as_str().unwrap_or(""),
                            result["url"].as_str().unwrap_or(""),
                            result["description"].as_str().unwrap_or(""),
                        ));
                    }
                }
                SearchBackend::Searxng { base_url } => {
                    let url = format!("{}/search", base_url.trim_end_matches('/'));
                    let response = client
                        .get(url)
                        .query(&[("q", args.query.as_str()), ("format", "json")])
                        .send()
                        .map_err(|error| format!("web_search: {error}"))?;
                    let status = response.status();
                    let body: serde_json::Value = response
                        .json()
                        .map_err(|error| format!("web_search: {error}"))?;
                    if !status.is_success() {
                        return Err(format!(
                            "web_search: searxng instance returned {status} (does it allow format=json?)"
                        ));
                    }
                    for (index, result) in body["results"]
                        .as_array()
                        .map(|results| results.as_slice())
                        .unwrap_or(&[])
                        .iter()
                        .take(count)
                        .enumerate()
                    {
                        results.push_str(&format!(
                            "{}. {}\n   {}\n   {}\n\n",
                            index + 1,
                            result["title"].as_str().unwrap_or(""),
                            result["url"].as_str().unwrap_or(""),
                            result["content"].as_str().unwrap_or(""),
                        ));
                    }
                }
            }

            if results.is_empty() {
                Ok("no results".into())
            } else {
                Ok(results)
            }
        })
    }
}

/// Full-text search over gertie's own past sessions.
pub struct SearchHistory {
    store: Arc<gertie_core::SessionStore>,
}

impl SearchHistory {
    pub fn new(store: Arc<gertie_core::SessionStore>) -> Self {
        Self { store }
    }
}

impl Tool for SearchHistory {
    fn name(&self) -> &'static str {
        "search_history"
    }

    fn description(&self) -> String {
        "Search past gertie sessions (your own conversation memory). \
         Returns session titles and matching snippets."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "text to look for"},
                "limit": {"type": "integer", "description": "max hits (default 10, max 50)"}
            },
            "required": ["query"]
        })
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        blocking(move || {
            #[derive(serde::Deserialize)]
            struct Args {
                query: String,
                limit: Option<usize>,
            }
            let args: Args = crate::parse(arguments, "search_history")?;
            let hits = self
                .store
                .search(&args.query, args.limit.unwrap_or(10).clamp(1, 50))
                .map_err(|error| format!("search_history: {error}"))?;
            if hits.is_empty() {
                return Ok("no matches in past sessions".into());
            }
            let mut out = String::new();
            for hit in hits {
                out.push_str(&format!(
                    "[{}] {} — {} #{}\n   {}\n\n",
                    hit.role, hit.session_title, hit.session_id.0, hit.message_id.0, hit.snippet
                ));
            }
            Ok(out)
        })
    }
}

/// Read back the open session's own record — the full text a context
/// compaction selected away, and everything before the model's current
/// window. Scoped by the front's session scope (the same cell worker
/// dispatch uses for lineage); read-only.
pub struct ReadSession {
    store: Arc<gertie_core::SessionStore>,
    scope: gertie_core::SessionScope,
}

impl ReadSession {
    pub fn new(store: Arc<gertie_core::SessionStore>, scope: gertie_core::SessionScope) -> Self {
        Self { store, scope }
    }
}

impl Tool for ReadSession {
    fn name(&self) -> &'static str {
        "read_session"
    }

    fn description(&self) -> String {
        "Read back earlier messages from the open session's record. \
         After a context compaction, the selection in your context is a \
         table of contents — this returns the full text of omitted turns. \
         Without a query you get the most recent messages in order; with \
         one, full-text matches from this session."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "optional full-text filter"},
                "limit": {"type": "integer", "description": "max messages (default 20, max 100)"}
            }
        })
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        blocking(move || {
            #[derive(serde::Deserialize)]
            struct Args {
                query: Option<String>,
                limit: Option<usize>,
            }
            let args: Args = crate::parse(arguments, "read_session")?;
            let Some(session) = self.scope.get() else {
                return Ok("no session is open — nothing to read".into());
            };
            let query = args
                .query
                .as_deref()
                .map(str::trim)
                .filter(|q| !q.is_empty());
            let hits = self
                .store
                .read_session(session, query, args.limit.unwrap_or(20).clamp(1, 100))
                .map_err(|error| format!("read_session: {error}"))?;
            if hits.is_empty() {
                return Ok("nothing in the session record for that".into());
            }
            let mut out = String::new();
            for hit in hits {
                out.push_str(&format!(
                    "[{} #{}] {}\n\n",
                    hit.role, hit.message_id.0, hit.snippet
                ));
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_session_reads_the_scoped_session_and_honors_the_query() {
        let store =
            Arc::new(gertie_core::SessionStore::open(std::path::Path::new(":memory:")).unwrap());
        let scope = gertie_core::SessionScope::new();
        let tool = ReadSession::new(store.clone(), scope.clone());
        let session = store.create_session("s").unwrap();
        store
            .append_message(
                session,
                None,
                gertie_core::Role::User,
                "hello from the record",
            )
            .unwrap();

        // Scope unset: an honest nothing, not an error.
        let out = futures_now(tool.run("{}")).unwrap();
        assert!(out.contains("no session is open"), "{out}");

        // Scope set: the record comes back.
        scope.set(Some(session));
        let out = futures_now(tool.run("{}")).unwrap();
        assert!(out.contains("hello from the record"), "{out}");

        // The query path scopes full-text, and an absent term says so.
        // Snippet markers (▶…◀) wrap the match — assert around them.
        let out = futures_now(tool.run(r#"{"query": "record"}"#)).unwrap();
        assert!(out.contains("hello from the"), "{out}");
        assert!(out.contains('▶'), "the match is marked: {out}");
        let out = futures_now(tool.run(r#"{"query": "absent-thing"}"#)).unwrap();
        assert!(out.contains("nothing in the session record"), "{out}");
    }

    /// Tests run outside a runtime; `blocking` futures are computed on
    /// first poll, so a plain poll loop suffices (same as mcp.rs).
    fn futures_now(
        mut future: std::pin::Pin<Box<dyn Future<Output = ToolResult> + Send + '_>>,
    ) -> ToolResult {
        use std::task::{Context, Poll, Waker};
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            match std::pin::Pin::new(&mut future).poll(&mut cx) {
                Poll::Ready(result) => return result,
                Poll::Pending => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
    }

    #[test]
    fn html_strips_to_readable_text() {
        let html = "<html><head><title>t</title><style>x{}</style></head>\
                    <body><script>evil()</script><h1>Heading</h1>\
                    <p>Para one &amp; two.</p><!-- comment --></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Heading"));
        assert!(text.contains("Para one & two."));
        assert!(!text.contains("evil"));
        assert!(!text.contains("comment"));
        assert!(!text.contains("<"));
    }
}
