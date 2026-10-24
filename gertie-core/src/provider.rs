//! The [`Provider`] contract. Lives in core because the agent loop drives
//! providers directly — implementations sit in gertie-providers (and
//! anywhere else: a test fake is a provider too).

use crate::request::Request;
use futures_core::stream::BoxStream;

/// A source of model completions. Implementations must own their retry
/// policy (backoff + jitter on 429/5xx/connection errors, never on
/// 401/403) and surface every retry to the event stream before failing.
pub trait Provider: Send + Sync {
    /// Streams one response as a sequence of [`Event`]s. The stream always
    /// ends with `Event::Done` or `Event::Error`. Dropping the stream
    /// cancels the request.
    fn stream(&self, request: Request) -> BoxStream<'static, crate::Event>;
}
