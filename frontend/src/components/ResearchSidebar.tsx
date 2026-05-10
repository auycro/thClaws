import { useEffect, useMemo, useRef, useState } from "react";
import { ChevronRight, X, Search } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";

/// M6.39.5: verbose right-edge sidebar for active research jobs.
/// Mirrors the visual rhythm of `PlanSidebar` / `TodoSidebar` but shows
/// `/research` pipeline progression in detail — current phase,
/// iteration-by-iteration score history, accumulated source count,
/// and a scrolling phase log so the user can watch the pipeline
/// think rather than just see "done" at the end.
///
/// The left-side Sidebar's Research panel still shows the list of
/// all jobs (compact); this right-side panel shows the most recent
/// active or completed one in detail.

type ResearchStatus =
  | "pending"
  | "running"
  | "done"
  | "cancelled"
  | "failed";

type ResearchJobInfo = {
  id: string;
  status: ResearchStatus;
  phase: string;
  query: string;
  iterations_done: number;
  source_count: number;
  last_score: number | null;
  kms_target: string | null;
  result_page: string | null;
  error: string | null;
  started_at: number | null;
  finished_at: number | null;
};

/// Per-job in-memory progression log accumulated by the frontend
/// from successive `research_update` broadcasts. The backend payload
/// only carries the latest snapshot; we keep a chronological log of
/// distinct phases + iteration milestones for the verbose view.
type JobProgress = {
  /// Most recent envelope shape — mirrors `ResearchJobInfo`.
  view: ResearchJobInfo;
  /// Distinct `phase` strings in order seen, deduped on consecutive
  /// repeats so a fast burst of identical phase updates doesn't
  /// crowd the log.
  phaseLog: string[];
  /// One entry per iteration completed, recording its end-of-loop
  /// score + source count delta. Empty during iteration 0;
  /// populated when `iterations_done` increments.
  iterationHistory: { iter: number; score: number | null; sourceCount: number }[];
};

const STATUS_COLOR: Record<ResearchStatus, string> = {
  pending: "var(--text-secondary)",
  running: "var(--warning, #d4a657)",
  done: "var(--accent)",
  cancelled: "var(--text-secondary)",
  failed: "var(--danger, #e06c75)",
};

const STATUS_LABEL: Record<ResearchStatus, string> = {
  pending: "queued",
  running: "running",
  done: "done",
  cancelled: "cancelled",
  failed: "failed",
};

export function ResearchSidebar() {
  // null = no envelope yet; the sidebar suppresses entirely on a
  // fresh session that's never seen a /research run.
  const [progressMap, setProgressMap] = useState<Map<string, JobProgress> | null>(null);
  // Which job's details to show. Defaults to "most recent active or
  // finished"; user can switch by clicking another job's chevron in
  // the left Sidebar (a future enhancement could surface a dropdown
  // here too).
  const [focusedId, setFocusedId] = useState<string | null>(null);
  const [dismissed, setDismissed] = useState(false);
  const lastSeenIterRef = useRef<Map<string, number>>(new Map());

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type !== "research_update") return;
      const jobs = (msg.jobs as ResearchJobInfo[]) ?? [];
      setProgressMap((prev) => {
        const next = new Map(prev ?? new Map());
        for (const j of jobs) {
          const existing = next.get(j.id);
          // Phase log: append distinct phase strings only.
          let phaseLog = existing?.phaseLog ?? [];
          if (
            phaseLog.length === 0 ||
            phaseLog[phaseLog.length - 1] !== j.phase
          ) {
            phaseLog = [...phaseLog, j.phase];
            // Cap length so a long-running job doesn't grow the
            // array unbounded.
            if (phaseLog.length > 40) {
              phaseLog = phaseLog.slice(phaseLog.length - 40);
            }
          }
          // Iteration history: append once per detected iteration
          // increment. Tracked outside React state so the comparison
          // doesn't fight reconciliation.
          let iterationHistory = existing?.iterationHistory ?? [];
          const lastIter = lastSeenIterRef.current.get(j.id) ?? 0;
          if (j.iterations_done > lastIter) {
            for (let n = lastIter + 1; n <= j.iterations_done; n++) {
              iterationHistory = [
                ...iterationHistory,
                {
                  iter: n,
                  score: n === j.iterations_done ? j.last_score : null,
                  sourceCount: j.source_count,
                },
              ];
            }
            lastSeenIterRef.current.set(j.id, j.iterations_done);
          }
          next.set(j.id, { view: j, phaseLog, iterationHistory });
        }
        // Drop entries for jobs the backend no longer reports — manager
        // pruning eventually removes terminal jobs older than its
        // retention window. Keeping them in our local map would show
        // ghosts.
        const liveIds = new Set(jobs.map((j) => j.id));
        for (const id of Array.from(next.keys())) {
          if (!liveIds.has(id)) {
            next.delete(id);
            lastSeenIterRef.current.delete(id);
          }
        }
        return next;
      });
      // Auto-focus selection: prefer the most recently-started
      // running job; fall back to most-recent terminal. User can
      // override (sticky once a job is selected manually — but we
      // don't expose manual selection in this revision).
      setFocusedId((prev) => {
        const prevStillExists = prev !== null && jobs.some((j) => j.id === prev);
        if (prevStillExists) return prev;
        const running = jobs.find((j) => j.status === "running" || j.status === "pending");
        if (running) return running.id;
        if (jobs.length > 0) return jobs[0].id;
        return null;
      });
      // Re-open if dismissed and a new running job appeared.
      if (jobs.some((j) => j.status === "running" || j.status === "pending")) {
        setDismissed(false);
      }
    });
    return unsub;
  }, []);

  const focused = useMemo(() => {
    if (!progressMap || !focusedId) return null;
    return progressMap.get(focusedId) ?? null;
  }, [progressMap, focusedId]);

  // Suppress entirely when no /research run has been observed.
  if (progressMap === null || progressMap.size === 0) return null;
  if (!focused) return null;

  // Collapsed: chevron tab on the right edge re-opens.
  if (dismissed) {
    return (
      <button
        type="button"
        onClick={() => setDismissed(false)}
        className="flex items-center justify-center shrink-0 border-l"
        style={{
          width: "20px",
          background: "var(--bg-secondary)",
          borderColor: "var(--border)",
          color: "var(--text-secondary)",
          cursor: "pointer",
        }}
        title={`Research: ${focused.view.status} · ${focused.view.iterations_done} iter${
          focused.view.last_score !== null ? ` · score ${focused.view.last_score.toFixed(2)}` : ""
        }`}
      >
        <ChevronRight size={14} style={{ transform: "rotate(180deg)" }} />
      </button>
    );
  }

  const { view, phaseLog, iterationHistory } = focused;
  const isRunning = view.status === "running" || view.status === "pending";

  // Iteration bar: 8 segments by default; if a custom max-iter
  // changed the cap we fallback to inferring from history length.
  // Backend doesn't ship max_iter in the envelope, so we use 8 as
  // the JobConfig default; if a real run goes past, we pad.
  const maxIter = Math.max(8, iterationHistory.length, view.iterations_done);

  const showResult = () => {
    send({ type: "chat_prompt", text: `/research show ${view.id}` });
  };
  const cancel = () => {
    send({ type: "chat_prompt", text: `/research cancel ${view.id}` });
  };

  return (
    <div
      className="flex flex-col shrink-0 border-l"
      style={{
        width: "280px",
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
      }}
    >
      {/* Header */}
      <div
        className="flex items-center justify-between px-3 py-2 border-b shrink-0"
        style={{ borderColor: "var(--border)" }}
      >
        <div
          className="text-[10px] uppercase tracking-wider flex items-center gap-2"
          style={{ color: "var(--text-secondary)" }}
        >
          <Search size={11} />
          <span>Research</span>
          <span
            className="px-1.5 py-px rounded font-mono"
            style={{
              fontSize: "9px",
              background: "var(--bg-tertiary)",
              color: STATUS_COLOR[view.status],
              border: `1px solid ${STATUS_COLOR[view.status]}`,
            }}
          >
            {STATUS_LABEL[view.status]}
          </span>
        </div>
        <button
          type="button"
          onClick={() => setDismissed(true)}
          className="p-0.5 rounded hover:bg-white/10"
          style={{ color: "var(--text-secondary)" }}
          title="Hide sidebar (job continues; restore via chevron)"
        >
          <X size={14} />
        </button>
      </div>

      {/* Body — scrollable */}
      <div className="flex-1 overflow-auto px-3 py-3 flex flex-col gap-3">
        {/* Query */}
        <div>
          <div
            className="text-[9px] uppercase tracking-wider mb-1"
            style={{ color: "var(--text-secondary)" }}
          >
            Query
          </div>
          <div
            className="text-xs leading-snug break-words"
            style={{ color: "var(--text-primary)" }}
          >
            {view.query}
          </div>
        </div>

        {/* Current phase (highlighted) */}
        <div>
          <div
            className="text-[9px] uppercase tracking-wider mb-1"
            style={{ color: "var(--text-secondary)" }}
          >
            Phase
          </div>
          <div
            className="text-xs leading-snug font-mono"
            style={{
              color: isRunning ? "var(--warning, #d4a657)" : "var(--text-primary)",
              fontWeight: isRunning ? 600 : 400,
            }}
          >
            {view.phase}
          </div>
        </div>

        {/* Iteration progress bar */}
        <div>
          <div
            className="text-[9px] uppercase tracking-wider mb-1 flex justify-between"
            style={{ color: "var(--text-secondary)" }}
          >
            <span>Iterations</span>
            <span>{view.iterations_done} / {maxIter}</span>
          </div>
          <div className="flex gap-0.5">
            {Array.from({ length: maxIter }).map((_, i) => {
              const completed = i < view.iterations_done;
              const inProgress = i === view.iterations_done && isRunning;
              return (
                <div
                  key={i}
                  className="flex-1 rounded-sm"
                  style={{
                    height: "6px",
                    background: completed
                      ? "var(--accent)"
                      : inProgress
                        ? "var(--warning, #d4a657)"
                        : "var(--bg-tertiary)",
                    border: "1px solid var(--border)",
                  }}
                />
              );
            })}
          </div>
        </div>

        {/* Iteration scores (one row per completed iter) */}
        {iterationHistory.length > 0 && (
          <div>
            <div
              className="text-[9px] uppercase tracking-wider mb-1"
              style={{ color: "var(--text-secondary)" }}
            >
              Score history
            </div>
            <div className="flex flex-col gap-0.5">
              {iterationHistory.map((h) => (
                <div
                  key={h.iter}
                  className="flex items-center gap-2 text-[10px] font-mono"
                  style={{ color: "var(--text-primary)" }}
                >
                  <span
                    style={{ color: "var(--text-secondary)", width: "26px" }}
                  >
                    iter {h.iter}
                  </span>
                  <div
                    className="flex-1 rounded-sm overflow-hidden"
                    style={{
                      height: "4px",
                      background: "var(--bg-tertiary)",
                      border: "1px solid var(--border)",
                    }}
                  >
                    {h.score !== null && (
                      <div
                        style={{
                          width: `${Math.max(0, Math.min(1, h.score)) * 100}%`,
                          height: "100%",
                          background: "var(--accent)",
                        }}
                      />
                    )}
                  </div>
                  <span style={{ width: "30px", textAlign: "right" }}>
                    {h.score !== null ? h.score.toFixed(2) : "—"}
                  </span>
                  <span
                    style={{
                      color: "var(--text-secondary)",
                      width: "44px",
                      textAlign: "right",
                    }}
                  >
                    {h.sourceCount} src
                  </span>
                </div>
              ))}
            </div>
          </div>
        )}

        {/* Sources accumulated */}
        <div className="flex items-center justify-between">
          <span
            className="text-[9px] uppercase tracking-wider"
            style={{ color: "var(--text-secondary)" }}
          >
            Sources accumulated
          </span>
          <span
            className="text-xs font-mono"
            style={{ color: "var(--text-primary)" }}
          >
            {view.source_count}
          </span>
        </div>

        {/* Phase log — chronological, deduped */}
        <div>
          <div
            className="text-[9px] uppercase tracking-wider mb-1"
            style={{ color: "var(--text-secondary)" }}
          >
            Phase log
          </div>
          <ul
            className="flex flex-col gap-0.5 font-mono"
            style={{ fontSize: "10px", color: "var(--text-secondary)" }}
          >
            {phaseLog.slice(-10).map((p, idx, arr) => {
              const isCurrent = idx === arr.length - 1;
              return (
                <li
                  key={`${p}-${idx}`}
                  className="leading-snug break-words"
                  style={{
                    color: isCurrent
                      ? "var(--text-primary)"
                      : "var(--text-secondary)",
                    opacity: isCurrent ? 1 : 0.7,
                  }}
                >
                  {isCurrent ? "→ " : "  "}
                  {p}
                </li>
              );
            })}
          </ul>
        </div>

        {/* Result / error footer */}
        {view.status === "done" && view.result_page && (
          <div>
            <div
              className="text-[9px] uppercase tracking-wider mb-1"
              style={{ color: "var(--text-secondary)" }}
            >
              Result
            </div>
            <div
              className="text-[10px] font-mono break-all"
              style={{ color: "var(--accent)" }}
            >
              {view.result_page}
            </div>
          </div>
        )}
        {view.status === "failed" && view.error && (
          <div>
            <div
              className="text-[9px] uppercase tracking-wider mb-1"
              style={{ color: "var(--danger, #e06c75)" }}
            >
              Error
            </div>
            <div
              className="text-[10px] break-words"
              style={{ color: "var(--danger, #e06c75)" }}
            >
              {view.error}
            </div>
          </div>
        )}
      </div>

      {/* Footer — actions */}
      <div
        className="px-3 py-2 border-t shrink-0 flex items-center gap-1.5"
        style={{
          borderColor: "var(--border)",
          fontSize: "10px",
          color: "var(--text-secondary)",
        }}
      >
        {view.status === "done" && (
          <button
            onClick={showResult}
            className="px-2 py-1 rounded font-medium"
            style={{
              background: "var(--accent)",
              color: "#fff",
            }}
            title="Print synthesized result in chat"
          >
            Show result
          </button>
        )}
        {isRunning && (
          <button
            onClick={cancel}
            className="px-2 py-1 rounded"
            style={{
              background: "var(--bg-tertiary)",
              color: "var(--text-secondary)",
              border: "1px solid var(--border)",
            }}
            title="Cancel this research job"
          >
            Cancel
          </button>
        )}
        <span className="ml-auto" style={{ opacity: 0.6 }}>
          {view.id}
        </span>
      </div>
    </div>
  );
}
