//! Test support for every crate in the workspace (tests outside a
//! runtime, polling futures that complete on other threads). Hidden
//! from docs: not part of the API, but shared, so each crate's tests
//! stop carrying their own copy.

use std::future::Future;
use std::time::{Duration, Instant};

/// Polls `future` to completion on this thread. A future still pending
/// after ten seconds fails the test by name instead of hanging it.
#[doc(hidden)]
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let started = Instant::now();
    loop {
        if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the future stayed Pending for 10s, an await never resolved"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
