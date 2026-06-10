/**
 * RunningChip — header indicator that an agent turn is in flight.
 *
 * Shows nothing when idle. When busy, shows a pulsing dot, elapsed
 * time, and the last `[i/N] subject — verdict` progress line the
 * engine extracted from the text stream. Click to attach to the
 * running session (sends `/load <id>` through the normal shell
 * input path).
 *
 * Companion to `useBusyState` — see dev-plan/36.
 */
import { useEffect, useState } from "react";
import { send } from "../hooks/useIPC";
import { useBusyState } from "../hooks/useBusyState";

function fmtElapsed(startedAtMs: number): string {
  const s = Math.max(0, Math.floor((Date.now() - startedAtMs) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  const rem = s % 60;
  if (m < 60) return `${m}m${rem}s`;
  const h = Math.floor(m / 60);
  return `${h}h${m % 60}m`;
}

export function RunningChip() {
  const { busy, sessionId, startedAtMs, lastProgress } = useBusyState();
  // Force a re-render every second so the elapsed-time label ticks
  // without polling the engine. Only mounts the interval when busy.
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!busy) return;
    const id = window.setInterval(() => setTick((t) => t + 1), 1000);
    return () => window.clearInterval(id);
  }, [busy]);

  if (!busy) return null;

  const elapsed = startedAtMs ? fmtElapsed(startedAtMs) : "";
  const onClick = () => {
    if (sessionId) {
      send({ type: "shell_input", text: `/load ${sessionId}` });
    }
  };

  return (
    <button
      onClick={onClick}
      title={
        sessionId
          ? `Running session ${sessionId} — click to attach`
          : "Agent running"
      }
      className="running-chip flex items-center gap-1.5 px-2 py-0.5 mr-2 rounded text-xs font-medium"
      style={{
        background: "rgba(95, 179, 179, 0.15)",
        color: "var(--accent, #5fb3b3)",
        border: "1px solid rgba(95, 179, 179, 0.45)",
        cursor: sessionId ? "pointer" : "default",
        maxWidth: 480,
      }}
    >
      <span
        className="running-chip-dot"
        style={{
          display: "inline-block",
          width: 7,
          height: 7,
          borderRadius: "50%",
          background: "currentColor",
        }}
      />
      <span>running</span>
      {elapsed && <span style={{ opacity: 0.8 }}>· {elapsed}</span>}
      {lastProgress && (
        <span
          style={{
            opacity: 0.75,
            fontFamily: "ui-monospace, SFMono-Regular, monospace",
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
            maxWidth: 320,
          }}
        >
          · {lastProgress}
        </span>
      )}
    </button>
  );
}
