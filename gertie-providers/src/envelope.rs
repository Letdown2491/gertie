//! What both HTTP wires share: one tokio runtime and one HTTP client
//! for the whole process, the retry + fallback envelope around a wire's
//! single attempt, and the SSE line splitter.
//!
//! The envelope: silent retries per endpoint, fallback across endpoints
//! until one produces content, commit on first content. A wire only
//! supplies its [`Attempt`]: build the body, POST, classify the status,
//! parse its own event shapes.
//!
//! Cancellation: the loop stops a turn by dropping its future, which
//! drops the returned stream. The spawned work races the receiver's
//! close, so a dropped stream aborts the in-flight request (and the
//! connection with it) instead of downloading, and paying for, the rest
//! of a reply nobody will read.

use crate::{Endpoint, Outcome, RequestScoped, Terminal};
use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use gertie_core::Event;
use gertie_core::request::Request;
use std::sync::OnceLock;
use std::time::Duration;
use tokio_stream::wrappers::UnboundedReceiverStream;

pub(crate) type Sender = tokio::sync::mpsc::UnboundedSender<Event>;

/// One HTTP attempt against one endpoint, the only wire-specific step.
pub(crate) type Attempt =
    for<'a> fn(&'a RequestScoped, &'a Request, &'a Sender) -> BoxFuture<'a, Outcome>;

/// A connect that has not finished in this long is a dead endpoint:
/// the retry (and then the fallback) gets its turn.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest silence between two reads of a live response. Generous:
/// a reasoning model can think a long while before its first token,
/// but a stream silent this long is stalled, not thinking.
const READ_TIMEOUT: Duration = Duration::from_secs(300);
/// A single SSE line longer than this is not an event, it is a broken
/// (or hostile) server: the buffer must not grow without bound.
const MAX_SSE_LINE: usize = 16 * 1024 * 1024;

struct Shared {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

/// The process's one runtime and client. Its own multi-thread runtime:
/// spawned tasks must progress without any caller entering it (the UI
/// polls the returned streams on gpui's executor). Shared, because a
/// provider is built per pinned agent and per workspace switch, and
/// each used to bring two worker threads and a connection pool.
fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| Shared {
        runtime: tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("gertie-net")
            .enable_all()
            .build()
            .expect("tokio runtime"),
        client: reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .expect("http client"),
    })
}

pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    &shared().runtime
}

pub(crate) fn client() -> reqwest::Client {
    shared().client.clone()
}

/// Spawns `work` on the shared runtime with a receiver for what it
/// sends. The work is dropped as soon as the receiver is, so nothing
/// keeps running for a stream nobody polls.
pub(crate) fn spawn_stream<F>(work: impl FnOnce(Sender) -> F) -> BoxStream<'static, Event>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    spawn_stream_on(runtime(), work)
}

/// [`spawn_stream`] on a given runtime (the replay wire keeps its own).
pub(crate) fn spawn_stream_on<F>(
    runtime: &tokio::runtime::Runtime,
    work: impl FnOnce(Sender) -> F,
) -> BoxStream<'static, Event>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let watch = sender.clone();
    let work = work(sender);
    runtime.spawn(async move {
        tokio::select! {
            () = work => {}
            () = watch.closed() => {}
        }
    });
    Box::pin(UnboundedReceiverStream::new(receiver))
}

/// The envelope around a wire's attempt: see the module docs.
pub(crate) fn stream(
    endpoints: Vec<Endpoint>,
    request: Request,
    attempt: Attempt,
) -> BoxStream<'static, Event> {
    let client = client();
    spawn_stream(move |sender| fall_back(endpoints, client, request, attempt, sender))
}

/// Aborts a spawned task when dropped: the per-endpoint driver must die
/// with the envelope that awaits it.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn fall_back(
    endpoints: Vec<Endpoint>,
    client: reqwest::Client,
    request: Request,
    attempt: Attempt,
    sender: Sender,
) {
    let request = std::sync::Arc::new(request);
    let mut last_error: Option<String> = None;
    for endpoint in endpoints {
        let scoped = RequestScoped {
            endpoint,
            client: client.clone(),
        };
        let (proxy, mut proxy_rx) = tokio::sync::mpsc::unbounded_channel();
        let request = request.clone();
        let mut driver = AbortOnDrop(tokio::spawn(async move {
            // The proxy sender is owned here, so its drop marks the end
            // of the stream for the forwarder below.
            with_retries(&scoped, &request, &proxy, attempt).await
        }));
        // Forward live, but hold everything back until this endpoint
        // has produced content: pre-content, the only events are
        // Started (the loop ignores it), Done (which would end the turn
        // early), and errors (held for the fallback decision).
        let mut produced = false;
        while let Some(event) = proxy_rx.recv().await {
            match &event {
                Event::Token { .. } | Event::ToolCall { .. } | Event::Usage { .. } => {
                    produced = true
                }
                _ if !produced => continue,
                _ => {}
            }
            if sender.send(event).is_err() {
                return; // the stream was dropped; the driver aborts with us
            }
        }
        if produced {
            // Committed to this endpoint; its whole stream (errors
            // after content included) already passed.
            return;
        }
        let outcome = (&mut driver.0)
            .await
            .unwrap_or_else(|_| Err(Terminal::Error("stream driver failed".into())));
        match outcome {
            // Clean end with no content: the server chose to send
            // nothing. Report Done, don't fall back.
            Ok(()) => {
                let _ = sender.send(Event::Done);
                return;
            }
            Err(Terminal::Error(message)) => last_error = Some(message),
            // A limit isn't an endpoint failure to fall back from. Other
            // endpoints share the same account. End the turn limited.
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
}

/// One endpoint's full retry policy. Ok when the stream completed, Err
/// with the reason once retries are exhausted (auth/config errors
/// return immediately). Retry delays are silent: an Event::Error is
/// terminal for the turn, so a "retrying" note would kill the turn it
/// was trying to save.
async fn with_retries(
    scoped: &RequestScoped,
    request: &Request,
    sender: &Sender,
    attempt: Attempt,
) -> Result<(), Terminal> {
    let mut tries = 0;
    loop {
        tries += 1;
        let _ = sender.send(Event::Started);
        let (message, limited) = match attempt(scoped, request, sender).await {
            Outcome::Done => return Ok(()),
            Outcome::Fatal(message) => return Err(Terminal::Error(message)),
            Outcome::Transient(message) => (message, false),
            Outcome::Limited(message) => (message, true),
        };
        if tries >= crate::MAX_ATTEMPTS {
            let message = format!("{message} (after {tries} attempts)");
            return Err(if limited {
                Terminal::Limited(message)
            } else {
                Terminal::Error(message)
            });
        }
        tokio::time::sleep(crate::backoff(tries)).await;
    }
}

/// Splits an SSE byte stream into lines, with the buffer bounded.
#[derive(Default)]
pub(crate) struct SseLines {
    buffer: Vec<u8>,
}

impl SseLines {
    /// Adds a chunk. Err when a line has outgrown [`MAX_SSE_LINE`].
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<(), String> {
        self.buffer.extend_from_slice(chunk);
        if self.buffer.len() > MAX_SSE_LINE && !self.buffer.contains(&b'\n') {
            self.buffer = Vec::new();
            return Err(format!(
                "the server sent a stream line over {} MB",
                MAX_SSE_LINE / (1024 * 1024)
            ));
        }
        Ok(())
    }

    /// The next complete line, without its terminator.
    pub(crate) fn next_line(&mut self) -> Option<String> {
        let end = self.buffer.iter().position(|byte| *byte == b'\n')?;
        let line: Vec<u8> = self.buffer.drain(..=end).collect();
        Some(String::from_utf8_lossy(&line).trim_end().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn sse_lines_split_and_stay_bounded() {
        let mut lines = SseLines::default();
        lines.push(b"data: a\r\ndata: b").unwrap();
        assert_eq!(lines.next_line().as_deref(), Some("data: a"));
        assert_eq!(lines.next_line(), None);
        lines.push(b"\n").unwrap();
        assert_eq!(lines.next_line().as_deref(), Some("data: b"));
        let huge = vec![b'x'; MAX_SSE_LINE + 1];
        assert!(lines.push(&huge).is_err());
        assert_eq!(lines.next_line(), None);
    }

    /// Dropping the stream stops the work: the Stop button's contract.
    #[test]
    fn a_dropped_stream_stops_its_work() {
        let (finished_tx, finished_rx) = std::sync::mpsc::channel::<()>();
        let stream = spawn_stream(move |sender| async move {
            let _guard = Signal(finished_tx);
            loop {
                if sender.send(Event::Started).is_err() {
                    // Never reached on the abort path; the select
                    // drops this future first.
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let mut stream = stream;
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let first = loop {
            if let std::task::Poll::Ready(event) = stream.poll_next_unpin(&mut cx) {
                break event;
            }
            assert!(std::time::Instant::now() < deadline, "no first event");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert!(matches!(first, Some(Event::Started)));
        drop(stream);
        finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the work was dropped with the stream");
    }

    /// Sends on drop, so the test sees the work's future go away.
    struct Signal(std::sync::mpsc::Sender<()>);
    impl Drop for Signal {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
}
