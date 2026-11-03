//! Web tools: fetch a page as text, search the web (when configured),
//! and search gertie's own session history.
//!
//! Boring choices: fetch is reqwest-blocking with a hard timeout and
//! runs under the same blocking seam as every other tool; HTML is
//! stripped to text (no headless browser, no JS); search backends are
//! pluggable and **gracefully disabled**, an unconfigured backend is a
//! clear model-visible message, never a broken turn.

use crate::{Tool, ToolResult, blocking};
use serde_json::json;
use std::io::Read as _;
use std::net::ToSocketAddrs as _;
use std::sync::Arc;

/// Matches the builtin tools' cap: one result may not eat the window.
const RESULT_CHAR_LIMIT: usize = 16_000;
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const USER_AGENT: &str = concat!("gertie/", env!("CARGO_PKG_VERSION"));

/// Strips HTML to readable text: noisy elements dropped whole, tags
/// become newlines, entities decoded, whitespace collapsed. Best-effort
///, the goal is model-readable text, not typographic fidelity.
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
pub struct WebFetch {
    /// Whether private-network targets may be fetched: loopback, the
    /// RFC 1918 ranges, link-local (cloud metadata), and friends. Off
    /// by default, the model has no business probing the user's
    /// router or a metadata endpoint, but a localhost dev server is a
    /// legitimate target for a coding agent, so the user's config can
    /// open the gate (`security.allow_private_fetch`).
    allow_private: bool,
}

impl WebFetch {
    pub fn new(allow_private: bool) -> Self {
        Self { allow_private }
    }

    /// The URL-level gate: scheme already checked, now the host. A
    /// literal private IP is refused outright; a localhost-ish name
    /// likewise; a public name is resolved and every answer checked,
    /// a public name over a private address is the same request
    /// wearing a disguise. The gate re-opens nothing on redirect:
    /// every hop dials through the same checked resolver.
    fn check_target(url: &reqwest::Url, allow_private: bool) -> Result<(), String> {
        if allow_private {
            return Ok(());
        }
        let host = url
            .host_str()
            .ok_or_else(|| "web_fetch: the URL names no host".to_string())?;
        // IPv6 literals arrive bracketed; parse the address, not the
        // punctuation.
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let reason = |what: &str| {
            format!(
                "web_fetch refuses {host}: {what}, the open web only, \
                 unless your config sets security.allow_private_fetch = true"
            )
        };
        if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
            if !publicly_fetchable(ip) {
                return Err(reason("a private-network address"));
            }
        } else if host == "localhost"
            || host.ends_with(".localhost")
            || host.ends_with(".local")
            || host.ends_with(".home.arpa")
        {
            return Err(reason("a private-network name"));
        } else {
            resolve_checked(host, false)?;
        }
        Ok(())
    }
}

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

    /// The card is the whole exfil gate. The fetch mutates nothing on
    /// disk, so mutates() alone would let it auto-run next to the
    /// read tools, and a fetched page is attacker-readable text that
    /// just asked a read tool for a secret: without a card, injected
    /// instructions get a zero-prompt send of that secret to any host
    /// the model names. The prompt shows the URL; the human sees the
    /// destination before the bytes leave.
    fn asks(&self) -> bool {
        true
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        let allow_private = self.allow_private;
        let arguments = arguments.to_string();
        blocking(move || {
            #[derive(serde::Deserialize)]
            struct Args {
                url: String,
            }
            let args: Args = crate::parse(&arguments, "web_fetch")?;
            let url: reqwest::Url = args
                .url
                .parse()
                .map_err(|_| format!("web_fetch: not a URL: {url}", url = args.url))?;
            // Scheme gate: no file://, no ftp://, no surprises.
            if url.scheme() != "http" && url.scheme() != "https" {
                return Err("only http and https URLs are fetched".into());
            }
            Self::check_target(&url, allow_private)?;

            let client = reqwest::blocking::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .user_agent(USER_AGENT)
                .redirect(reqwest::redirect::Policy::custom(redirect_hop))
                .dns_resolver(Arc::new(PublicDnsOnly { allow_private }))
                .build()
                .map_err(|error| format!("web_fetch: {error}"))?;
            let response = client
                .get(url)
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

/// True when an address names somewhere the open web lives. Private,
/// loopback, link-local, and reserved ranges are refused by default:
/// the model has no business probing the user's router, an internal
/// service, or a cloud-metadata endpoint, and a public name that
/// resolves into one of these is the same request wearing a disguise.
fn publicly_fetchable(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            !(ip.is_private() // RFC 1918
                || ip.is_loopback()
                || ip.is_link_local() // 169.254/16: APIPA, cloud metadata
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_multicast()
                || ip.octets()[0] == 0 // 0.0.0.0/8, "this network"
                || ip.octets()[0] == 100
                    && (64..=127).contains(&ip.octets()[1]) // 100.64/10: CGNAT, tailnets
                || ip.octets()[0] == 198
                    && (18..=19).contains(&ip.octets()[1]) // benchmarking
                || ip.octets()[0] >= 240) // reserved, incl. broadcast
        }
        std::net::IpAddr::V6(ip) => {
            // An IPv4 address in IPv6 clothes is still that IPv4
            // address, mapped and NAT64-embedded forms are unwrapped
            // and judged as their v4 selves.
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return publicly_fetchable(std::net::IpAddr::V4(mapped));
            }
            let segments = ip.segments();
            if segments[0] == 0x64 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0] {
                let v4 = std::net::Ipv4Addr::new(
                    (segments[6] >> 8) as u8,
                    segments[6] as u8,
                    (segments[7] >> 8) as u8,
                    segments[7] as u8,
                );
                return publicly_fetchable(std::net::IpAddr::V4(v4));
            }
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local() // fc00::/7
                || ip.is_unicast_link_local() // fe80::/10
                || ip.is_multicast())
        }
    }
}

/// Resolve a host, refusing any answer that is not publicly routable.
/// Blocking resolution is fine here: the fetch client is per-call and
/// the call already runs on the blocking seam.
fn resolve_checked(host: &str, allow_private: bool) -> Result<Vec<std::net::SocketAddr>, String> {
    let addrs: Vec<_> = (host, 0)
        .to_socket_addrs()
        .map_err(|error| format!("web_fetch: could not resolve {host}: {error}"))?
        .collect();
    if !allow_private {
        for addr in &addrs {
            if !publicly_fetchable(addr.ip()) {
                return Err(format!(
                    "web_fetch refuses {host}: it resolves to {addr}, a \
                     private-network address, the open web only, unless \
                     your config sets security.allow_private_fetch = true"
                ));
            }
        }
    }
    Ok(addrs)
}

/// The DNS resolver behind the fetch client: it hands out only
/// publicly routable addresses, so every connection, the first fetch
/// and every redirect hop, is re-checked at the moment it dials. The
/// gap between "checked the name" and "dialed the address" has no
/// room for a rebinding server to live in.
struct PublicDnsOnly {
    allow_private: bool,
}

impl reqwest::dns::Resolve for PublicDnsOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let allow_private = self.allow_private;
        Box::pin(std::future::ready(
            resolve_checked(&host, allow_private)
                .map(|addrs| -> reqwest::dns::Addrs { Box::new(addrs.into_iter()) })
                .map_err(Box::<dyn std::error::Error + Send + Sync>::from),
        ))
    }
}

/// Redirect hops stay http or https and stay bounded. Exotic schemes
/// are refused rather than followed; every hop's address is checked
/// by the resolver anyway.
fn redirect_hop(attempt: reqwest::redirect::Attempt) -> reqwest::redirect::Action {
    let scheme = attempt.url().scheme().to_owned();
    if scheme != "http" && scheme != "https" {
        return attempt.error(format!(
            "web_fetch follows only http and https redirects, not {scheme}:"
        ));
    }
    if attempt.previous().len() >= 10 {
        return attempt.error("web_fetch: too many redirects");
    }
    attempt.follow()
}

/// Web search. Configured backends: Brave (API key) and SearXNG
/// (self-hosted or public instance allowing JSON). Unconfigured, the
/// tool exists and explains itself, the model can tell the user what
/// to add rather than failing mysteriously.
pub struct WebSearch {
    backend: Option<SearchBackend>,
}

#[derive(Clone)]
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
        let backend = self.backend.clone();
        let arguments = arguments.to_string();
        blocking(move || {
            #[derive(serde::Deserialize)]
            struct Args {
                query: String,
                count: Option<usize>,
            }
            let args: Args = crate::parse(&arguments, "web_search")?;
            let count = args.count.unwrap_or(5).clamp(1, 10);

            let Some(backend) = &backend else {
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
                            "web_search: brave returned {status}, {}",
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
        let store = self.store.clone();
        let arguments = arguments.to_string();
        blocking(move || {
            #[derive(serde::Deserialize)]
            struct Args {
                query: String,
                limit: Option<usize>,
            }
            let args: Args = crate::parse(&arguments, "search_history")?;
            let hits = store
                .search(&args.query, args.limit.unwrap_or(10).clamp(1, 50))
                .map_err(|error| format!("search_history: {error}"))?;
            if hits.is_empty() {
                return Ok("no matches in past sessions".into());
            }
            let mut out = String::new();
            for hit in hits {
                out.push_str(&format!(
                    "[{}] {}, {} #{}\n   {}\n\n",
                    hit.role, hit.session_title, hit.session_id.0, hit.message_id.0, hit.snippet
                ));
            }
            Ok(out)
        })
    }
}

/// Read back the open session's own record, the full text a context
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
         table of contents, this returns the full text of omitted turns. \
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
        let store = self.store.clone();
        let scope = self.scope.clone();
        let arguments = arguments.to_string();
        blocking(move || {
            #[derive(serde::Deserialize)]
            struct Args {
                query: Option<String>,
                limit: Option<usize>,
            }
            let args: Args = crate::parse(&arguments, "read_session")?;
            let Some(session) = scope.get() else {
                return Ok("no session is open, nothing to read".into());
            };
            let query = args
                .query
                .as_deref()
                .map(str::trim)
                .filter(|q| !q.is_empty());
            let hits = store
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
        // Snippet markers (▶…◀) wrap the match, assert around them.
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
        let started = std::time::Instant::now();
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            match std::pin::Pin::new(&mut future).poll(&mut cx) {
                Poll::Ready(result) => return result,
                Poll::Pending => {
                    // The hang guard: a future that never resolves must
                    // fail the test fast and by name, never spin
                    // politely forever.
                    if started.elapsed() > std::time::Duration::from_secs(10) {
                        panic!("the future stayed Pending for 10s, an await never resolved");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
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

    #[test]
    fn the_fetch_predicate_judges_addresses_not_strings() {
        let public: &[&str] = &[
            "8.8.8.8",
            "93.184.216.34",
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
        ];
        for ip in public {
            assert!(
                publicly_fetchable(ip.parse().unwrap()),
                "{ip} is publicly routable"
            );
        }
        let private: &[&str] = &[
            "127.0.0.1",        // loopback
            "10.1.2.3",         // RFC 1918
            "192.168.0.1",      // RFC 1918
            "172.31.255.255",   // RFC 1918
            "169.254.169.254",  // link-local: the cloud-metadata ask
            "0.0.0.0",          // unspecified
            "100.64.0.1",       // CGNAT / tailnet
            "100.127.255.255",  // CGNAT's last address
            "198.18.0.1",       // benchmarking
            "203.0.113.9",      // documentation
            "224.0.0.1",        // multicast
            "255.255.255.255",  // broadcast
            "240.0.0.1",        // reserved
            "::1",              // v6 loopback
            "::",               // v6 unspecified
            "fd00::1",          // v6 unique-local
            "fe80::1",          // v6 link-local
            "ff02::1",          // v6 multicast
            "::ffff:10.0.0.5",  // private v4 in v6 clothes
            "::ffff:127.0.0.1", // loopback in v6 clothes
            "64:ff9b::a00:1",   // NAT64-embedded private v4 (10.0.0.1)
        ];
        for ip in private {
            assert!(
                !publicly_fetchable(ip.parse().unwrap()),
                "{ip} is not publicly routable"
            );
        }
        // The CGNAT band's edges: 100.64/10 spans 100.64–100.127, so
        // 100.63 and 100.128 are ordinary public space.
        assert!(publicly_fetchable("100.63.255.255".parse().unwrap()));
        assert!(publicly_fetchable("100.128.0.1".parse().unwrap()));
    }

    #[test]
    fn the_fetch_gate_refuses_private_targets() {
        let url = |text: &str| text.parse::<reqwest::Url>().unwrap();
        // Literal private addresses, by range and by disguise.
        for target in [
            "http://127.0.0.1:8080/dev",
            "http://10.0.0.5/",
            "http://192.168.1.1/router",
            "http://172.16.0.9/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.100.1.1/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:10.0.0.5]/",
        ] {
            assert!(
                WebFetch::check_target(&url(target), false).is_err(),
                "must refuse {target}"
            );
        }
        // Private names: known without touching the network.
        for target in ["http://localhost/dev", "http://printer.local/"] {
            assert!(
                WebFetch::check_target(&url(target), false).is_err(),
                "must refuse {target}"
            );
        }
        // The gate opens when the user's config opens it.
        assert!(WebFetch::check_target(&url("http://127.0.0.1:8080/dev"), true).is_ok());
        // Literal public addresses pass with no DNS at all.
        assert!(WebFetch::check_target(&url("http://93.184.216.34/"), false).is_ok());
    }

    /// The fetch owes a card: the model chose the destination, and the
    /// body may carry the session's secrets out. Read-only in the
    /// filesystem sense only, so mutates() alone would never ask.
    #[test]
    fn web_fetch_sits_on_the_ask_path() {
        assert!(WebFetch::new(false).asks());
        assert!(WebFetch::new(true).asks());
    }

    #[test]
    fn the_dns_resolver_refuses_private_answers() {
        // A literal loopback "resolves" from the local tables with no
        // network, enough to prove the filter answers for the dial.
        assert!(resolve_checked("127.0.0.1", false).is_err());
        assert!(resolve_checked("127.0.0.1", true).is_ok());
    }
}
