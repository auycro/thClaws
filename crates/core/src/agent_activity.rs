//! Process-wide "is the agent currently working?" signal + the
//! who/when/what metadata the UI uses to surface it.
//!
//! `drive_turn_stream` wraps every `Agent::run_turn` invocation. By
//! holding a `BusyGuard` for the lifetime of that wrapper we keep a
//! counter > 0 while any turn is in flight (including nested side-
//! channel turns for reconcile / ingest / subagents — they all funnel
//! through `drive_turn_stream`).
//!
//! Two consumers:
//!
//! 1. **Cloud heartbeat** (`server.rs::spawn_cloud_heartbeat`) — reads
//!    `is_agent_busy()` to decide whether to ping `/keepalive` when no
//!    WS client is attached. Keeps the cloud reaper from pausing a
//!    pod mid-batch.
//!
//! 2. **Running-jobs UI** (dev-plan/36) — reads `busy_meta()` for the
//!    session id / start time / last-progress line the workspace UI
//!    chip and the cloud-dashboard pill display. The chip auto-loads
//!    the running session on browser reconnect.
//!
//! The counter and the metadata are kept in sync via the same RAII
//! guard. Process-global by design: there is exactly one engine
//! process per workspace; the heartbeat is per-process; the UI's
//! "what's running" is per-process. Threading an
//! `Arc<Mutex<BusyMeta>>` through every `WorkerState` construction
//! path would be noisier than a static and buy nothing.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use tokio::sync::Notify;

static AGENT_BUSY_COUNT: AtomicUsize = AtomicUsize::new(0);

static BUSY_META: Mutex<Option<BusyMeta>> = Mutex::new(None);

/// Fired on every transition of the user-facing busy state — both the
/// idle→busy edge (`for_session` constructs the first meta) and the
/// busy→idle edge (the owning guard drops). The cloud heartbeat
/// listens on this so a busy transition triggers an immediate
/// `/keepalive` POST instead of waiting up to 60s for the next
/// periodic tick — without this the dashboard "running" pill lagged
/// by up to a minute behind the actual turn.
static BUSY_TRANSITION: LazyLock<Arc<Notify>> = LazyLock::new(|| Arc::new(Notify::new()));

/// Shared notify the heartbeat task awaits to learn about busy
/// transitions. Cloning is cheap (`Arc`) — keep one per subscriber.
pub fn busy_transition() -> Arc<Notify> {
    BUSY_TRANSITION.clone()
}

/// Snapshot of who's running, since when, and the last user-visible
/// progress line. The UI displays this in the "running" chip; the
/// cloud dashboard aggregates the `busy` boolean.
///
/// `session_id` is the *user-facing* session that initiated the turn.
/// Side-channel turns (ingest, reconcile, subagent fan-out) increment
/// the counter but do NOT overwrite the surface meta — the UI keeps
/// pointing at the user's session, which is what they actually want
/// to land in on reconnect.
#[derive(Debug, Clone)]
pub struct BusyMeta {
    pub session_id: String,
    pub started_at: SystemTime,
    pub last_progress: Option<String>,
}

/// RAII guard — increment on construction, decrement on drop. Use at
/// the top of any code path that should count as "agent doing work."
/// Drop runs on every return path, including panic-unwind, so this
/// stays correct around the many early `return`s in
/// `drive_turn_stream` (cancel, error, end-of-stream).
///
/// Constructed via `for_session(...)` for the user-facing turn (sets
/// `BUSY_META`) or `for_side_channel()` for ingest/reconcile/subagent
/// turns (counter only, no meta overwrite).
pub struct BusyGuard {
    /// True if this guard set `BUSY_META` on construction and must
    /// clear it on drop. Side-channel guards skip both ends.
    owns_meta: bool,
}

impl BusyGuard {
    /// Construct for a user-facing turn. Stashes `BusyMeta` so the
    /// UI chip can surface the session id + start time. Drops the
    /// meta on guard drop.
    ///
    /// **Auto-degrade on nesting:** if `BUSY_META` is already set
    /// (an outer user-facing turn is in flight), this constructor
    /// acts like `for_side_channel` — the counter increments but
    /// the meta is NOT overwritten. The UI keeps pointing at the
    /// outer turn the user is actually watching. The
    /// `nested_for_session_does_not_overwrite_outer_meta` test pins
    /// this contract.
    pub fn for_session(session_id: impl Into<String>) -> Self {
        AGENT_BUSY_COUNT.fetch_add(1, Ordering::SeqCst);
        let mut slot = BUSY_META.lock().expect("BUSY_META poisoned");
        if slot.is_some() {
            // Nested under another user-facing turn — leave meta alone.
            return Self { owns_meta: false };
        }
        *slot = Some(BusyMeta {
            session_id: session_id.into(),
            started_at: SystemTime::now(),
            last_progress: None,
        });
        // Drop the lock before notifying so any waiter that wakes and
        // immediately reads `busy_meta()` doesn't contend with us.
        drop(slot);
        BUSY_TRANSITION.notify_waiters();
        Self { owns_meta: true }
    }

    /// Construct for a side-channel turn (ingest, reconcile, nested
    /// subagent). Increments the counter so the heartbeat sees us as
    /// busy, but leaves `BUSY_META` alone — the UI keeps pointing at
    /// the user-facing turn that's already running.
    pub fn for_side_channel() -> Self {
        AGENT_BUSY_COUNT.fetch_add(1, Ordering::SeqCst);
        Self { owns_meta: false }
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        AGENT_BUSY_COUNT.fetch_sub(1, Ordering::SeqCst);
        if self.owns_meta {
            *BUSY_META.lock().expect("BUSY_META poisoned") = None;
            // Wake heartbeat waiters so the busy=false ping fires
            // immediately on turn end instead of after up to 60s.
            // The dashboard pill clears within the 90s server-side
            // stickiness window from the last busy=true ping; the
            // sooner this notify fires the better.
            BUSY_TRANSITION.notify_waiters();
        }
    }
}

/// Is at least one agent turn currently in flight in this process?
pub fn is_agent_busy() -> bool {
    AGENT_BUSY_COUNT.load(Ordering::SeqCst) > 0
}

/// Current in-flight turn count. Mainly for tests + future
/// observability; the heartbeat just wants the boolean.
pub fn busy_count() -> usize {
    AGENT_BUSY_COUNT.load(Ordering::SeqCst)
}

/// Snapshot of the user-facing turn's metadata. `None` when no
/// user-facing turn is in flight (even if side-channel turns are —
/// the surface signal is the user's, not the engine's internals).
pub fn busy_meta() -> Option<BusyMeta> {
    BUSY_META.lock().expect("BUSY_META poisoned").clone()
}

/// Update the `last_progress` field of the current user-facing turn.
/// No-op when no user-facing turn is in flight. Called from
/// `drive_turn_stream` whenever it sees a `[i/N] subject — verdict`
/// line in the text stream (mirrors the GUI shell's progress regex).
pub fn update_progress(line: impl Into<String>) {
    let mut slot = BUSY_META.lock().expect("BUSY_META poisoned");
    if let Some(meta) = slot.as_mut() {
        meta.last_progress = Some(line.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests share the process-global counter + meta. Serialize them
    // with a mutex so parallel test runs (the cargo default) don't
    // see each other's guards as background noise. Each test resets
    // state on entry.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn reset() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        AGENT_BUSY_COUNT.store(0, Ordering::SeqCst);
        *BUSY_META.lock().unwrap() = None;
        guard
    }

    #[test]
    fn guard_increments_and_decrements() {
        let _lock = reset();
        {
            let _g = BusyGuard::for_side_channel();
            assert_eq!(busy_count(), 1);
            assert!(is_agent_busy());
        }
        assert_eq!(busy_count(), 0);
    }

    #[test]
    fn nested_guards_stack() {
        let _lock = reset();
        let _outer = BusyGuard::for_side_channel();
        {
            let _inner = BusyGuard::for_side_channel();
            assert_eq!(busy_count(), 2);
        }
        assert_eq!(busy_count(), 1);
    }

    #[test]
    fn guard_survives_early_return() {
        let _lock = reset();
        fn maybe_work(do_work: bool) -> Option<()> {
            let _g = BusyGuard::for_side_channel();
            if !do_work {
                return None;
            }
            Some(())
        }
        let _ = maybe_work(false);
        assert_eq!(busy_count(), 0, "early return must drop guard");
        let _ = maybe_work(true);
        assert_eq!(busy_count(), 0);
    }

    #[test]
    fn for_session_sets_meta() {
        let _lock = reset();
        assert!(busy_meta().is_none());
        {
            let _g = BusyGuard::for_session("sess-abc");
            let m = busy_meta().expect("meta set");
            assert_eq!(m.session_id, "sess-abc");
            assert!(m.last_progress.is_none());
        }
        assert!(busy_meta().is_none(), "meta cleared on drop");
    }

    #[test]
    fn side_channel_does_not_overwrite_meta() {
        let _lock = reset();
        let _outer = BusyGuard::for_session("user-session");
        {
            let _inner = BusyGuard::for_side_channel();
            let m = busy_meta().expect("meta from outer guard");
            assert_eq!(m.session_id, "user-session");
        }
        // Outer still alive — meta still points at user-session.
        assert_eq!(busy_meta().unwrap().session_id, "user-session");
    }

    #[test]
    fn update_progress_writes_into_active_meta() {
        let _lock = reset();
        update_progress("ignored — no active turn");
        assert!(busy_meta().is_none());

        let _g = BusyGuard::for_session("sess-progress");
        update_progress("[3/10] otter — done");
        let m = busy_meta().unwrap();
        assert_eq!(m.last_progress.as_deref(), Some("[3/10] otter — done"));

        update_progress("[4/10] owl — done");
        let m = busy_meta().unwrap();
        assert_eq!(m.last_progress.as_deref(), Some("[4/10] owl — done"));
    }

    #[test]
    fn nested_for_session_does_not_overwrite_outer_meta() {
        let _lock = reset();
        let _outer = BusyGuard::for_session("outer-session");
        {
            // Even though the inner caller passes a session id, the
            // outer turn's meta is the one the UI should keep
            // pointing at — the inner is a nested subagent or
            // workflow worker, not what the user is watching.
            let _inner = BusyGuard::for_session("inner-session");
            assert_eq!(
                busy_meta().unwrap().session_id,
                "outer-session",
                "outer meta wins; inner doesn't clobber"
            );
        }
        assert_eq!(busy_meta().unwrap().session_id, "outer-session");
    }
}
