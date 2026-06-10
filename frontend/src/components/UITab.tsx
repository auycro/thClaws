import { useState } from "react";
import { ArrowLeft } from "lucide-react";
import { UIPicker } from "./UIPicker";
import { UIView } from "./UIView";
import { useBusyState } from "../hooks/useBusyState";

// dev-plan/33 Tier 2 — single-instance container for the UI tab
// (formerly "Shell" — renamed when the new PTY-backed `Shell` tab
// claimed that name). Holds the picker until the user selects a GUI
// shell, then mounts the iframe via UIView. A small "Back" header
// button returns to the picker (the GUI-shell session persists;
// reopening from picker resumes it in Tier 2b once per-shell session
// ids are wired).
//
// Multi-instance UI tabs (N concurrent GUI shells in N tabs) is Task 13.

interface UITabProps {
  active: boolean;
  /** When the workspace tab bar is hidden (full-screen UI mode), we
   * also hide the in-tab "‹ shells / <id>" breadcrumb. Otherwise a
   * stray click on "shells" jumps back to the picker mid-workflow —
   * easy to do by accident when the shell takes the full viewport. */
  fullscreen?: boolean;
}

export function UITab({ active, fullscreen = false }: UITabProps) {
  const [selected, setSelected] = useState<string | null>(null);
  // Once the user has gone back to the picker (via the breadcrumb),
  // we want the grid even if settings.json::guiShell.tabDefault is set
  // — otherwise they'd be looped straight back to the default.
  const [skipDefault, setSkipDefault] = useState(false);
  // Also hide the breadcrumb while an agent turn is in flight, even
  // outside full-screen. A click on "‹ shells" mid-batch swaps the
  // iframe, which the user perceives as "the agent stopped" — but
  // the engine kept running and the lost shell state confuses
  // recovery. The chip pattern still surfaces the running session
  // in the header; the breadcrumb only matters when the user is
  // free to navigate.
  const busy = useBusyState();
  const hideBreadcrumb = fullscreen || busy.busy;

  if (selected === null) {
    return (
      <UIPicker
        onSelect={setSelected}
        honourDefault={!skipDefault}
      />
    );
  }

  return (
    <div className="w-full h-full flex flex-col">
      {!hideBreadcrumb && (
        <div
          className="flex items-center gap-2 px-3 py-1.5 text-xs border-b"
          style={{
            background: "var(--bg-secondary)",
            borderColor: "var(--border)",
            color: "var(--text-secondary)",
          }}
        >
          <button
            onClick={() => {
              setSkipDefault(true);
              setSelected(null);
            }}
            className="flex items-center gap-1 hover:underline"
            title="Return to shell picker"
          >
            <ArrowLeft size={12} /> shells
          </button>
          <span style={{ color: "var(--text-secondary)" }}>/</span>
          <span className="font-mono" style={{ color: "var(--text-primary)" }}>
            {selected}
          </span>
        </div>
      )}
      <div className="flex-1 min-h-0">
        <UIView active={active} shellId={selected} />
      </div>
    </div>
  );
}
