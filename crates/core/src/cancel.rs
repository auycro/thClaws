//! Cooperative cancellation token shared between the worker loop and
//! the agent. Pairs a sync `AtomicBool` (cheap polling check) with an
//! async `Notify` (interruptable wait), so callers can:
//!
//! - Poll `is_cancelled()` synchronously at loop boundaries
//! - `select!` against `cancelled().await` to interrupt long awaits
//!   (provider streams, retry sleeps, tool execution)
//!
//! Contract:
//! - `cancel()` flips the flag AND wakes every active `cancelled()` await
//! - `reset()` flips the flag back; in-flight `cancelled()` futures
//!   resolve before they observe the reset (Notify is fire-and-forget,
//!   not level-triggered) but new awaits start clean
//!
//! Replaces the bare `Arc<AtomicBool>` previously held by `WorkerState`.
//! Pre-fix the worker checked the flag only between stream events, so a
//! cancel during a slow tool call or stalled provider stream took
//! seconds-to-minutes to fire. M6.17 BUGs H1 + M3.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

#[derive(Clone, Default, Debug)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronous: has cancel been requested?
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Request cancellation. Sets the flag AND wakes every current
    /// `cancelled().await`. Idempotent — calling twice is fine; the
    /// second `notify_waiters()` is a no-op.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Clear the cancel state for the next operation. Call AFTER
    /// handling a cancellation, before starting the next user turn.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    /// Async: resolve when cancel is requested. Checks the flag first
    /// so an already-cancelled token returns immediately without
    /// awaiting Notify (which is one-shot). Use inside `tokio::select!`
    /// to interrupt long awaits.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        // The notify may fire spuriously (Notify::notify_waiters wakes
        // all current waiters); re-check the flag and re-wait if it
        // wasn't actually cancelled. In practice we only call cancel()
        // alongside notify_waiters(), so this loop terminates fast.
        loop {
            self.notify.notified().await;
            if self.is_cancelled() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_returns_immediately_when_already_cancelled() {
        let token = CancelToken::new();
        token.cancel();
        // Should resolve fast (no real wait).
        let res = tokio::time::timeout(Duration::from_millis(50), token.cancelled()).await;
        assert!(res.is_ok(), "cancelled() should return immediately when flag set");
    }

    #[tokio::test]
    async fn cancelled_wakes_when_cancel_called_while_waiting() {
        let token = CancelToken::new();
        let token2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token2.cancel();
        });
        let res = tokio::time::timeout(Duration::from_millis(200), token.cancelled()).await;
        assert!(res.is_ok(), "cancelled() should wake within timeout");
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn select_against_long_sleep() {
        // Pin: the canonical use case — a long sleep raced against
        // cancel. Without CancelToken's async wakeup we'd have to wait
        // the full sleep duration before the cancel could be observed.
        let token = CancelToken::new();
        let token2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token2.cancel();
        });
        let started = std::time::Instant::now();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("should have been cancelled"),
            _ = token.cancelled() => {}
        }
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn reset_clears_flag() {
        let token = CancelToken::new();
        token.cancel();
        assert!(token.is_cancelled());
        token.reset();
        assert!(!token.is_cancelled());
    }
}
