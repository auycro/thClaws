import { useEffect, useState } from "react";
import { ChevronRight, X } from "lucide-react";
import { subscribe } from "../hooks/useIPC";

/// Background-agents sidebar. Subscribes to the same
/// `chat_side_channel_*` envelopes ChatView consumes but renders a
/// persistent right-edge column showing which side-channel agents
/// (e.g. /dream) are currently running, how long they've been at it,
/// and — when they finish — a brief audit line. The inline chat bubble
/// can scroll out of view during a long /dream; this sidebar is the
/// "is it still running?" answer.

type AgentStatus = "running" | "done" | "error";

type AgentEntry = {
  id: string;
  agentName: string;
  status: AgentStatus;
  startedAt: number;
  finishedAt?: number;
  durationMs?: number;
  lastTool?: string;
  result?: string;
  error?: string;
};

/// Finished/errored entries linger this long so the user can read the
/// outcome before they vanish. Running entries stay forever (until
/// they themselves transition to done/error).
const FINISHED_TTL_MS = 5 * 60 * 1000;

const STATUS_ICON: Record<AgentStatus, string> = {
  running: "◉",
  done: "✓",
  error: "✗",
};

const STATUS_COLOR: Record<AgentStatus, string> = {
  running: "var(--accent)",
  done: "var(--text-secondary)",
  error: "#e57373",
};

function formatElapsed(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  const rem = s % 60;
  if (m < 60) return `${m}m ${rem.toString().padStart(2, "0")}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${(m % 60).toString().padStart(2, "0")}m`;
}

/// Parse a `dream-YYYY-MM-DD` page name out of the agent's final
/// status message. Dream's prompt asks it to end with the summary
/// page name so the user can jump to it; we surface that hint in the
/// sidebar. Returns null if the agent isn't dream or the page name
/// isn't present.
function summaryPageHint(agentName: string, result?: string): string | null {
  if (!result) return null;
  if (agentName !== "dream") return null;
  const m = result.match(/dream-\d{4}-\d{2}-\d{2}/);
  return m ? m[0] : null;
}

export function BackgroundAgentsSidebar() {
  const [agents, setAgents] = useState<Record<string, AgentEntry>>({});
  const [dismissed, setDismissed] = useState(false);
  /// Wall-clock state read by the render path for elapsed-time
  /// display. Updated by a ticker effect (1s while any agent is
  /// running, 30s while only finished entries linger for the TTL
  /// prune). Keeping `now` in state — rather than calling Date.now()
  /// in render — keeps the render function pure (react-hooks/purity).
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    const unsub = subscribe((msg) => {
      switch (msg.type) {
        case "chat_side_channel_start": {
          const id = String(msg.id ?? "");
          const agentName = String(msg.agent_name ?? "");
          if (!id) break;
          setAgents((prev) => ({
            ...prev,
            [id]: {
              id,
              agentName,
              status: "running",
              startedAt: Date.now(),
            },
          }));
          /// A fresh agent re-opens the dismissed sidebar — the user
          /// asked for it implicitly by invoking the slash command.
          setDismissed(false);
          break;
        }
        case "chat_side_channel_tool_call": {
          const id = String(msg.id ?? "");
          const toolName = String(msg.tool_name ?? "");
          if (!id || !toolName) break;
          setAgents((prev) => {
            const entry = prev[id];
            if (!entry) return prev;
            return { ...prev, [id]: { ...entry, lastTool: toolName } };
          });
          break;
        }
        case "chat_side_channel_done": {
          const id = String(msg.id ?? "");
          if (!id) break;
          const durationMs = Number(msg.duration_ms ?? 0);
          const result = String(msg.result_text ?? "");
          setAgents((prev) => {
            const entry = prev[id];
            if (!entry) return prev;
            return {
              ...prev,
              [id]: {
                ...entry,
                status: "done",
                finishedAt: Date.now(),
                durationMs,
                result,
              },
            };
          });
          break;
        }
        case "chat_side_channel_error": {
          const id = String(msg.id ?? "");
          const error = String(msg.error ?? "unknown error");
          if (!id) break;
          setAgents((prev) => {
            const entry = prev[id];
            if (!entry) return prev;
            return {
              ...prev,
              [id]: {
                ...entry,
                status: "error",
                finishedAt: Date.now(),
                error,
              },
            };
          });
          break;
        }
      }
    });
    return unsub;
  }, []);

  /// Single ticker effect. Re-runs whenever `agents` changes — i.e.
  /// when an agent spawns, transitions to done/error, or gets pruned.
  /// Fast tick (1s) while at least one is running so elapsed-time
  /// updates visibly; slow tick (30s) while only finished entries
  /// linger for the TTL prune. No entries → no interval. Both
  /// setState calls happen inside the timer callback (not
  /// synchronously in the effect body) so React 19's
  /// `set-state-in-effect` rule passes.
  useEffect(() => {
    const entries = Object.values(agents);
    if (entries.length === 0) return;
    const hasRunning = entries.some((a) => a.status === "running");
    const intervalMs = hasRunning ? 1000 : 30000;
    const id = window.setInterval(() => {
      const t = Date.now();
      setNow(t);
      setAgents((prev) => {
        let changed = false;
        const next: Record<string, AgentEntry> = {};
        for (const [k, entry] of Object.entries(prev)) {
          if (
            entry.status !== "running" &&
            entry.finishedAt !== undefined &&
            t - entry.finishedAt > FINISHED_TTL_MS
          ) {
            changed = true;
            continue;
          }
          next[k] = entry;
        }
        return changed ? next : prev;
      });
    }, intervalMs);
    return () => window.clearInterval(id);
  }, [agents]);

  const entries = Object.values(agents).sort(
    (a, b) => b.startedAt - a.startedAt,
  );

  if (entries.length === 0) return null;

  const runningCount = entries.filter((e) => e.status === "running").length;

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
          color:
            runningCount > 0 ? "var(--accent)" : "var(--text-secondary)",
          cursor: "pointer",
        }}
        title={
          runningCount > 0
            ? `${runningCount} background agent${
                runningCount > 1 ? "s" : ""
              } running`
            : `${entries.length} recent background agent${
                entries.length > 1 ? "s" : ""
              }`
        }
      >
        <ChevronRight size={14} style={{ transform: "rotate(180deg)" }} />
      </button>
    );
  }

  return (
    <div
      className="flex flex-col shrink-0 border-l"
      style={{
        width: "260px",
        background: "var(--bg-secondary)",
        borderColor: "var(--border)",
      }}
    >
      <div
        className="flex items-center justify-between px-3 py-2 border-b shrink-0"
        style={{ borderColor: "var(--border)" }}
      >
        <div
          className="text-[10px] uppercase tracking-wider flex items-center gap-2"
          style={{ color: "var(--text-secondary)" }}
        >
          <span>Background</span>
          {runningCount > 0 && (
            <span
              className="px-1.5 py-px rounded"
              style={{
                fontSize: "9px",
                background: "var(--accent)",
                color: "var(--bg-primary)",
                fontWeight: 600,
              }}
              title={`${runningCount} agent${
                runningCount > 1 ? "s" : ""
              } running`}
            >
              {runningCount} RUNNING
            </span>
          )}
        </div>
        <button
          type="button"
          onClick={() => setDismissed(true)}
          className="p-0.5 rounded hover:bg-white/10"
          style={{ color: "var(--text-secondary)" }}
          title="Hide sidebar (agents keep running in background)"
        >
          <X size={14} />
        </button>
      </div>

      <div className="flex-1 overflow-auto">
        <ul className="px-3 py-2 space-y-2.5">
          {entries.map((entry) => {
            const elapsedMs =
              entry.status === "running"
                ? now - entry.startedAt
                : entry.durationMs ?? 0;
            const pageHint = summaryPageHint(entry.agentName, entry.result);
            return (
              <li
                key={entry.id}
                className="flex flex-col gap-1 text-xs leading-snug pb-2 border-b"
                style={{
                  borderColor: "var(--border)",
                  opacity: entry.status === "done" ? 0.85 : 1,
                }}
              >
                <div className="flex items-start gap-2">
                  <span
                    className="font-mono shrink-0"
                    style={{
                      color: STATUS_COLOR[entry.status],
                      width: "14px",
                      textAlign: "center",
                    }}
                  >
                    {STATUS_ICON[entry.status]}
                  </span>
                  <span
                    style={{
                      fontWeight: entry.status === "running" ? 600 : 500,
                      color: "var(--text-primary)",
                    }}
                  >
                    /{entry.agentName}
                  </span>
                  <span
                    className="ml-auto font-mono"
                    style={{
                      color: "var(--text-secondary)",
                      fontSize: "10px",
                    }}
                  >
                    {formatElapsed(elapsedMs)}
                  </span>
                </div>
                {entry.status === "running" && entry.lastTool && (
                  <div
                    className="pl-5 font-mono"
                    style={{
                      color: "var(--text-secondary)",
                      fontSize: "10px",
                    }}
                  >
                    ↳ {entry.lastTool}
                  </div>
                )}
                {entry.status === "done" && pageHint && (
                  <div
                    className="pl-5 font-mono"
                    style={{
                      color: "var(--text-secondary)",
                      fontSize: "10px",
                    }}
                    title="Open this KMS page to review the dream summary"
                  >
                    → {pageHint}
                  </div>
                )}
                {entry.status === "error" && entry.error && (
                  <div
                    className="pl-5"
                    style={{
                      color: "#e57373",
                      fontSize: "10px",
                      wordBreak: "break-word",
                    }}
                  >
                    {entry.error.split("\n")[0]}
                  </div>
                )}
              </li>
            );
          })}
        </ul>
      </div>

      <div
        className="px-3 py-1.5 border-t shrink-0 flex items-center justify-between"
        style={{
          borderColor: "var(--border)",
          fontSize: "10px",
          color: "var(--text-secondary)",
        }}
      >
        <span>
          {entries.length} agent{entries.length > 1 ? "s" : ""}
          {runningCount > 0 && (
            <span style={{ color: "var(--accent)", marginLeft: "6px" }}>
              · {runningCount} running
            </span>
          )}
        </span>
        <span style={{ opacity: 0.6 }}>side-channel</span>
      </div>
    </div>
  );
}
