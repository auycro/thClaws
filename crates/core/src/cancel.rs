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
    /// Optional parent token. When set, this token is "downstream":
    /// `is_cancelled()` returns true if either own OR parent flag is
    /// set, and `cancelled().await` resolves on either's notify.
    /// `cancel()` only flips OWN flag — child cancel does NOT
    /// propagate up to parent. Used for spawning subagents on
    /// independent tokens: parent's Ctrl-C kills all children, but
    /// one child's failure doesn't kill siblings.
    parent_flag: Option<Arc<AtomicBool>>,
    parent_notify: Option<Arc<Notify>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn a child cancel that observes this parent's cancellation
    /// transitively but doesn't propagate its own cancel back up.
    /// Parent's `cancel()` → child's `is_cancelled()` returns true.
    /// Child's `cancel()` → parent unchanged, siblings unchanged.
    pub fn child(&self) -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
            parent_flag: Some(self.flag.clone()),
            parent_notify: Some(self.notify.clone()),
        }
    }

    /// Synchronous: has cancel been requested? Checks own flag and,
    /// if this is a child token, the parent's flag too.
    pub fn is_cancelled(&self) -> bool {
        if self.flag.load(Ordering::SeqCst) {
            return true;
        }
        match &self.parent_flag {
            Some(parent) => parent.load(Ordering::SeqCst),
            None => false,
        }
    }

    /// Request cancellation. Sets the flag AND wakes every current
    /// `cancelled().await`. Idempotent — calling twice is fine; the
    /// second `notify_waiters()` is a no-op. Only flips OWN flag —
    /// does not propagate to parent (siblings stay alive).
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Clear the cancel state for the next operation. Call AFTER
    /// handling a cancellation, before starting the next user turn.
    /// Only resets OWN flag — parent's flag (if this is a child) is
    /// not affected, since a parent that's been cancelled stays
    /// cancelled until its own owner resets it.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    /// Async: resolve when cancel is requested. Checks flags first
    /// so an already-cancelled token returns immediately without
    /// awaiting Notify (which is one-shot). Use inside `tokio::select!`
    /// to interrupt long awaits. Child tokens wake on either own or
    /// parent's notify.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        // The notify may fire spuriously (Notify::notify_waiters wakes
        // all current waiters); re-check the flag and re-wait if it
        // wasn't actually cancelled. In practice we only call cancel()
        // alongside notify_waiters(), so this loop terminates fast.
        loop {
            match &self.parent_notify {
                Some(parent_notify) => {
                    tokio::select! {
                        _ = self.notify.notified() => {}
                        _ = parent_notify.notified() => {}
                    }
                }
                None => {
                    self.notify.notified().await;
                }
            }
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
        assert!(
            res.is_ok(),
            "cancelled() should return immediately when flag set"
        );
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

    #[test]
    fn child_observes_parent_cancel() {
        // Parent → child relationship: parent cancel propagates down,
        // child cancel doesn't propagate up. Used for side-channel
        // subagents — Cmd-C in main kills all, one child's failure
        // doesn't kill siblings.
        let parent = CancelToken::new();
        let child = parent.child();
        assert!(!child.is_cancelled());
        parent.cancel();
        assert!(parent.is_cancelled());
        assert!(
            child.is_cancelled(),
            "parent cancel must propagate to child"
        );
    }

    #[test]
    fn child_cancel_does_not_propagate_to_parent() {
        let parent = CancelToken::new();
        let child = parent.child();
        child.cancel();
        assert!(child.is_cancelled());
        assert!(
            !parent.is_cancelled(),
            "child cancel must NOT propagate up to parent"
        );
    }

    #[test]
    fn sibling_children_are_independent() {
        let parent = CancelToken::new();
        let child_a = parent.child();
        let child_b = parent.child();
        child_a.cancel();
        assert!(child_a.is_cancelled());
        assert!(
            !child_b.is_cancelled(),
            "sibling cancels must not affect each other"
        );
        assert!(!parent.is_cancelled());
    }

    #[tokio::test]
    async fn child_cancelled_wakes_on_parent_cancel() {
        // Critical contract: a child awaiting `cancelled()` must wake
        // when the parent fires cancel — not just when its own flag
        // is flipped. Otherwise a side-channel subagent's retry sleep
        // wouldn't observe a Cmd-C in main.
        let parent = CancelToken::new();
        let child = parent.child();
        let child2 = child.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            parent.cancel();
        });
        let res = tokio::time::timeout(Duration::from_millis(200), child2.cancelled()).await;
        assert!(res.is_ok(), "child cancelled() must wake on parent cancel");
        assert!(child2.is_cancelled());
    }

    #[tokio::test]
    async fn child_cancelled_wakes_on_own_cancel() {
        let parent = CancelToken::new();
        let child = parent.child();
        let child2 = child.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            child.cancel();
        });
        let res = tokio::time::timeout(Duration::from_millis(200), child2.cancelled()).await;
        assert!(res.is_ok(), "child cancelled() must wake on own cancel");
    }
}
