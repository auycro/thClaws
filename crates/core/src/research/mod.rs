//! Background research pipeline (M6.39).
//!
//! `/research <query>` spawns a background tokio task that:
//! 1. runs an initial broad WebSearch
//! 2. asks the LLM to extract subtopics from query + seed
//! 3. iterates: parallel search per subtopic → fetch top-3 → accumulate
//! 4. asks the LLM to score completeness (0.0-1.0) + free-form notes
//! 5. if score < threshold (default 0.80) and iter < max, generate
//!    next-round subtopics from notes, loop
//! 6. on exit (score reached / max_iter / cancel), synthesize markdown
//!    with `[N]` citations, write to KMS as a timestamped note + update
//!    a `_summary.md` consolidated view
//!
//! Hard floor (`min_iter`, default 2) prevents the LLM from saying "good
//! enough" too early; hard ceiling (`max_iter`, default 8) prevents
//! runaway. Score threshold lives between the two as the LLM-driven
//! middle condition.
//!
//! Pipeline runs entirely outside the agent loop — direct
//! `Provider::stream()` calls, direct `WebSearchTool::call`. No tool
//! approval prompts (user authorized at `/research start`); no goal_state
//! singleton (avoids the M6.29 scope problem); no agent context
//! pollution (results land in KMS, not chat).
//!
//! Phase split: this module ships in M6.39.1 with mocked LLM/search
//! tests. M6.39.2 wires `/research` slash + REPL surface; M6.39.3 adds
//! the GUI sidebar panel.

pub mod kms_writer;
pub mod llm_calls;
pub mod pipeline;
#[cfg(test)]
pub(crate) mod test_helpers;

use crate::cancel::CancelToken;
use crate::error::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Stable ID for a background research job. Format `research-<8-hex>`.
pub type JobId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// Spawned but pipeline hasn't started yet (rare — spawn → run is
    /// near-instant, but we transition through this so the job is
    /// queryable the moment `start` returns).
    Pending,
    /// Running. `JobView::phase` is the current step.
    Running,
    /// Pipeline completed normally — synthesized result is in KMS,
    /// `JobView::result_path` points at the raw note.
    Done,
    /// User called `cancel`, or main process is shutting down.
    Cancelled,
    /// Pipeline aborted with an error. Inspect `JobView::error`.
    Failed,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Pending => "pending",
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Failed => "failed",
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Done | JobStatus::Cancelled | JobStatus::Failed
        )
    }
}

/// Tunable knobs for one research job. Defaults aim for "decent answer
/// in ~3-5 minutes with a mid-tier model"; user overrides via
/// `/research --min-iter / --max-iter / --score-threshold`.
#[derive(Debug, Clone)]
pub struct JobConfig {
    /// Minimum iterations before the score threshold can short-circuit
    /// the loop. Hard floor — Rust ignores any "score >= threshold"
    /// signal before this. Default 2.
    pub min_iter: u32,
    /// Maximum iterations regardless of score. Hard ceiling. Default 8.
    pub max_iter: u32,
    /// LLM-reported completeness score that ends the loop early
    /// (between `min_iter` and `max_iter`). Range 0.0-1.0. Default 0.80.
    pub score_threshold: f32,
    /// Subtopics per iteration. Default 4 — balances breadth vs cost.
    pub subtopics_per_iter: u32,
    /// Top-N pages per subtopic to read in detail (WebFetch). Default 3.
    pub fetch_top_n: u32,
    /// M6.39.6: cap on KMS pages emitted per research run. Plan step
    /// asks the LLM to group sources into ≤ this many pages. Default
    /// 7 — enough granularity for entity / concept / comparison
    /// pages without exploding LLM cost (each page is one LLM call).
    /// Override via `/research --max-pages N`.
    pub max_pages: u32,
    /// Per-LLM-call timeout. Default 900s (15 min) — research
    /// synthesis is a known long-running feature; this also feeds the
    /// provider's per-chunk idle ceiling via
    /// `StreamRequest::stream_chunk_timeout_override`, so it bypasses
    /// the user's normal `stream_chunk_timeout_secs` setting.
    pub llm_timeout: Duration,
    /// Total wall-clock budget. Pipeline aborts as `Failed` with a
    /// budget-exhausted message past this. Default 15 minutes.
    pub time_budget: Duration,
    /// Target KMS name. `None` = auto-derive a slug from the query and
    /// create the KMS if missing.
    pub kms_target: Option<String>,
}

impl Default for JobConfig {
    fn default() -> Self {
        Self {
            min_iter: 2,
            max_iter: 8,
            score_threshold: 0.80,
            subtopics_per_iter: 4,
            fetch_top_n: 3,
            max_pages: 7,
            llm_timeout: Duration::from_secs(900),
            time_budget: Duration::from_secs(15 * 60),
            kms_target: None,
        }
    }
}

/// Read-only snapshot of one job for status / list views.
#[derive(Debug, Clone)]
pub struct JobView {
    pub id: JobId,
    pub query: String,
    pub status: JobStatus,
    pub phase: String,
    pub iterations_done: u32,
    pub source_count: u32,
    /// Most recent `score: 0.X` from `evaluate()`. `None` until iter 1
    /// finishes.
    pub last_score: Option<f32>,
    pub started_at: std::time::SystemTime,
    pub finished_at: Option<std::time::SystemTime>,
    pub kms_target: Option<String>,
    /// On `Done`, the relative path inside the target KMS (e.g.
    /// `2026-05-09-obon-festival.md`). `None` until completion.
    pub result_page: Option<String>,
    pub error: Option<String>,
}

/// Thread-safe registry of running + recently-completed jobs.
///
/// Ownership: process-wide singleton, accessed via [`manager`]. Same
/// pattern as `goal_state` and `schedule` for symmetry, but each
/// research job carries its own `CancelToken` so cancellation is
/// per-job, not global.
pub struct ResearchManager {
    jobs: RwLock<HashMap<JobId, Arc<RwLock<JobInner>>>>,
    /// Broadcaster wired by `gui.rs` at session bootstrap (M6.39.3).
    /// Each phase change / iteration record / finalize fires this
    /// with a JSON payload shape identical to what
    /// `build_research_update_payload()` produces. CLI uses are no-op
    /// (broadcaster unset) — phases are visible via `/research list`
    /// and the auto-print on next REPL prompt.
    broadcaster: Mutex<Option<Box<dyn Fn(&[JobView]) + Send + Sync>>>,
}

#[derive(Debug)]
struct JobInner {
    view: JobView,
    cancel: CancelToken,
    /// Wall-clock deadline derived from `JobConfig::time_budget`.
    /// Pipeline checks this before each iteration; on overrun, sets
    /// `JobStatus::Failed` with `error = "time budget exhausted"`.
    deadline: Instant,
}

impl Default for ResearchManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ResearchManager {
    pub fn new() -> Self {
        Self {
            jobs: RwLock::new(HashMap::new()),
            broadcaster: Mutex::new(None),
        }
    }

    /// Install a broadcaster called after every state-mutating method
    /// (update_phase, record_iteration, finalize, cancel). The closure
    /// receives a fresh snapshot of all jobs in newest-first order.
    /// Same pattern as `goal_state::set_broadcaster`.
    pub fn set_broadcaster<F>(&self, f: F)
    where
        F: Fn(&[JobView]) + Send + Sync + 'static,
    {
        *self.broadcaster.lock().unwrap() = Some(Box::new(f));
    }

    fn broadcast(&self) {
        if let Some(cb) = self.broadcaster.lock().unwrap().as_ref() {
            let snapshot = self.list();
            cb(&snapshot);
        }
    }

    /// Register a new job and return its ID + cancel handle. Caller
    /// then drives the pipeline asynchronously and reports progress
    /// via [`update_phase`] / [`finalize`].
    pub fn register(&self, query: String, config: &JobConfig) -> (JobId, CancelToken) {
        let id = format!(
            "research-{}",
            uuid::Uuid::new_v4()
                .to_string()
                .split('-')
                .next()
                .unwrap_or("anon")
                .to_string()
        );
        let cancel = CancelToken::new();
        let inner = JobInner {
            view: JobView {
                id: id.clone(),
                query,
                status: JobStatus::Pending,
                phase: "spawned".into(),
                iterations_done: 0,
                source_count: 0,
                last_score: None,
                started_at: std::time::SystemTime::now(),
                finished_at: None,
                kms_target: config.kms_target.clone(),
                result_page: None,
                error: None,
            },
            cancel: cancel.clone(),
            deadline: Instant::now() + config.time_budget,
        };
        self.jobs
            .write()
            .unwrap()
            .insert(id.clone(), Arc::new(RwLock::new(inner)));
        self.broadcast();
        (id, cancel)
    }

    pub fn update_phase(&self, id: &str, phase: impl Into<String>) {
        let changed = if let Some(j) = self.jobs.read().unwrap().get(id).cloned() {
            let mut g = j.write().unwrap();
            g.view.phase = phase.into();
            g.view.status = JobStatus::Running;
            true
        } else {
            false
        };
        if changed {
            self.broadcast();
        }
    }

    pub fn record_iteration(
        &self,
        id: &str,
        iteration: u32,
        source_count: u32,
        score: Option<f32>,
    ) {
        let changed = if let Some(j) = self.jobs.read().unwrap().get(id).cloned() {
            let mut g = j.write().unwrap();
            g.view.iterations_done = iteration;
            g.view.source_count = source_count;
            // Always assign — `None` clears, `Some` sets. Pre-fix
            // this only assigned on `Some`, which caused the previous
            // iter's score to bleed into the new iter's broadcast
            // snapshots: pipeline records the iter-start broadcast
            // before evaluate runs, so during that window
            // iterations_done points at iter N while last_score still
            // carries iter N-1's value. The frontend's per-iter
            // history then labelled the wrong score under iter N.
            // Pipeline now passes `None` at iter-start (no eval yet)
            // and the real score at the post-evaluate broadcast.
            g.view.last_score = score;
            true
        } else {
            false
        };
        if changed {
            self.broadcast();
        }
    }

    /// Mark a job terminal (Done / Cancelled / Failed) with optional
    /// result path or error. Idempotent — second call on a terminal
    /// job is a no-op.
    pub fn finalize(
        &self,
        id: &str,
        status: JobStatus,
        result_page: Option<String>,
        error: Option<String>,
    ) {
        debug_assert!(
            status.is_terminal(),
            "finalize called with non-terminal status"
        );
        let changed = if let Some(j) = self.jobs.read().unwrap().get(id).cloned() {
            let mut g = j.write().unwrap();
            if g.view.status.is_terminal() {
                false
            } else {
                g.view.status = status;
                g.view.result_page = result_page;
                g.view.error = error;
                g.view.finished_at = Some(std::time::SystemTime::now());
                g.view.phase = match status {
                    JobStatus::Done => "done".into(),
                    JobStatus::Cancelled => "cancelled".into(),
                    JobStatus::Failed => "failed".into(),
                    _ => g.view.phase.clone(),
                };
                true
            }
        } else {
            false
        };
        if changed {
            self.broadcast();
        }
    }

    /// Snapshot of one job. `None` if the id isn't known.
    pub fn get(&self, id: &str) -> Option<JobView> {
        self.jobs
            .read()
            .unwrap()
            .get(id)
            .map(|j| j.read().unwrap().view.clone())
    }

    /// Snapshot of every job (running + recently completed). Sorted by
    /// `started_at` descending so `/research list` shows newest first.
    pub fn list(&self) -> Vec<JobView> {
        let mut all: Vec<JobView> = self
            .jobs
            .read()
            .unwrap()
            .values()
            .map(|j| j.read().unwrap().view.clone())
            .collect();
        all.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        all
    }

    /// Signal a job's CancelToken. Pipeline checks the token between
    /// each await point and exits as `JobStatus::Cancelled`. Returns
    /// `false` if the id isn't known or the job is already terminal.
    pub fn cancel(&self, id: &str) -> bool {
        if let Some(j) = self.jobs.read().unwrap().get(id).cloned() {
            let g = j.read().unwrap();
            if g.view.status.is_terminal() {
                return false;
            }
            g.cancel.cancel();
            return true;
        }
        false
    }

    /// True if the job has exceeded its wall-clock budget. Pipeline
    /// checks this each iteration to short-circuit before running
    /// another expensive round of LLM + search calls.
    pub fn is_over_budget(&self, id: &str) -> bool {
        self.jobs
            .read()
            .unwrap()
            .get(id)
            .map(|j| Instant::now() > j.read().unwrap().deadline)
            .unwrap_or(false)
    }

    /// Drop terminal jobs older than `keep_recent`. Called periodically
    /// by the manager owner (or on `/research list` to clean up).
    pub fn prune_terminal(&self, keep_recent: Duration) {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(keep_recent)
            .unwrap_or(std::time::UNIX_EPOCH);
        self.jobs.write().unwrap().retain(|_, j| {
            let g = j.read().unwrap();
            !g.view.status.is_terminal() || g.view.finished_at.map(|t| t > cutoff).unwrap_or(true)
        });
    }
}

/// Process-wide manager. Lazy-initialized on first access.
pub fn manager() -> &'static ResearchManager {
    use std::sync::OnceLock;
    static M: OnceLock<ResearchManager> = OnceLock::new();
    M.get_or_init(ResearchManager::new)
}

/// Public entry point used by the slash dispatch in M6.39.2. Builds
/// the job, spawns the pipeline as a background task, returns the
/// JobId immediately. The caller doesn't await the task — status is
/// queried via the manager.
///
/// `provider` and `tools` are taken by Arc so the spawned task owns
/// independent handles. The pipeline closes over them for the duration
/// of the job.
pub async fn start(
    query: String,
    config: JobConfig,
    provider: Arc<dyn crate::providers::Provider>,
    model: String,
) -> Result<JobId> {
    let (id, cancel) = manager().register(query.clone(), &config);
    let id_for_task = id.clone();
    let cfg_for_task = config.clone();
    tokio::spawn(async move {
        let outcome = pipeline::run(
            &id_for_task,
            query,
            cfg_for_task,
            provider,
            model,
            cancel.clone(),
        )
        .await;
        match outcome {
            Ok(result_page) => {
                manager().finalize(&id_for_task, JobStatus::Done, Some(result_page), None);
            }
            Err(e) => {
                let s = format!("{e}");
                let status = if cancel.is_cancelled() {
                    JobStatus::Cancelled
                } else {
                    JobStatus::Failed
                };
                manager().finalize(&id_for_task, status, None, Some(s));
            }
        }
    });
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_creates_pending_job_with_unique_id() {
        let mgr = ResearchManager::new();
        let cfg = JobConfig::default();
        let (a, _) = mgr.register("query a".into(), &cfg);
        let (b, _) = mgr.register("query b".into(), &cfg);
        assert_ne!(a, b);
        assert!(a.starts_with("research-"));
        assert_eq!(mgr.get(&a).unwrap().status, JobStatus::Pending);
    }

    #[test]
    fn update_phase_flips_to_running() {
        let mgr = ResearchManager::new();
        let (id, _) = mgr.register("q".into(), &JobConfig::default());
        mgr.update_phase(&id, "iteration 1");
        let v = mgr.get(&id).unwrap();
        assert_eq!(v.status, JobStatus::Running);
        assert_eq!(v.phase, "iteration 1");
    }

    #[test]
    fn finalize_is_idempotent_on_terminal_job() {
        let mgr = ResearchManager::new();
        let (id, _) = mgr.register("q".into(), &JobConfig::default());
        mgr.finalize(&id, JobStatus::Done, Some("note.md".into()), None);
        mgr.finalize(&id, JobStatus::Failed, None, Some("late err".into()));
        let v = mgr.get(&id).unwrap();
        assert_eq!(v.status, JobStatus::Done);
        assert_eq!(v.result_page.as_deref(), Some("note.md"));
        assert!(v.error.is_none());
    }

    #[test]
    fn cancel_signals_token_and_returns_true() {
        let mgr = ResearchManager::new();
        let (id, cancel) = mgr.register("q".into(), &JobConfig::default());
        assert!(!cancel.is_cancelled());
        assert!(mgr.cancel(&id));
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn cancel_returns_false_for_terminal_job() {
        let mgr = ResearchManager::new();
        let (id, _) = mgr.register("q".into(), &JobConfig::default());
        mgr.finalize(&id, JobStatus::Done, None, None);
        assert!(!mgr.cancel(&id));
    }

    #[test]
    fn cancel_returns_false_for_unknown_id() {
        let mgr = ResearchManager::new();
        assert!(!mgr.cancel("research-nope"));
    }

    #[test]
    fn list_sorted_newest_first() {
        let mgr = ResearchManager::new();
        let cfg = JobConfig::default();
        let (a, _) = mgr.register("first".into(), &cfg);
        std::thread::sleep(Duration::from_millis(10));
        let (b, _) = mgr.register("second".into(), &cfg);
        let v = mgr.list();
        assert_eq!(v[0].id, b);
        assert_eq!(v[1].id, a);
    }

    #[test]
    fn record_iteration_updates_counters() {
        let mgr = ResearchManager::new();
        let (id, _) = mgr.register("q".into(), &JobConfig::default());
        mgr.record_iteration(&id, 1, 8, Some(0.4));
        mgr.record_iteration(&id, 2, 17, Some(0.78));
        let v = mgr.get(&id).unwrap();
        assert_eq!(v.iterations_done, 2);
        assert_eq!(v.source_count, 17);
        assert_eq!(v.last_score, Some(0.78));
    }

    #[test]
    fn job_status_terminal_classification() {
        assert!(!JobStatus::Pending.is_terminal());
        assert!(!JobStatus::Running.is_terminal());
        assert!(JobStatus::Done.is_terminal());
        assert!(JobStatus::Cancelled.is_terminal());
        assert!(JobStatus::Failed.is_terminal());
    }

    #[test]
    fn default_config_matches_documented_knobs() {
        // Pin the defaults — these are user-facing and changing them
        // shifts the `/research` UX. Update this test deliberately if
        // we tune them.
        let c = JobConfig::default();
        assert_eq!(c.min_iter, 2);
        assert_eq!(c.max_iter, 8);
        assert!((c.score_threshold - 0.80).abs() < f32::EPSILON);
        assert_eq!(c.subtopics_per_iter, 4);
        assert_eq!(c.fetch_top_n, 3);
        assert_eq!(c.max_pages, 7);
    }
}
