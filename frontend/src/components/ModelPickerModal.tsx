import { useEffect, useMemo, useRef, useState } from "react";
import { send } from "../hooks/useIPC";

/// One row from the catalogue, as the backend ships it.
export type PickerModel = {
  id: string;
  context?: number | null;
  max_output?: number | null;
};

type Props = {
  provider: string;
  current: string;
  models: PickerModel[];
  onClose: () => void;
};

/// Format a context window in a tight human form: 200_000 → "200k", etc.
function formatCtx(n: number | null | undefined): string {
  if (!n || n <= 0) return "";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
  if (n >= 1_000) return `${Math.round(n / 1000)}k`;
  return String(n);
}

/// Post-key-entry model picker. Opens when the backend broadcasts
/// `model_picker_open` after a successful api_key_set for a provider with
/// a non-trivial catalogue. The user picks a default; we send model_set;
/// the modal closes. Skipping leaves auto_fallback_model's choice in place.
export function ModelPickerModal({ provider, current, models, onClose }: Props) {
  const [query, setQuery] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  // Esc to skip.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return models;
    return models.filter((m) => m.id.toLowerCase().includes(q));
  }, [models, query]);

  const pick = (id: string) => {
    send({ type: "model_set", model: id });
    onClose();
  };

  return (
    <div
      className="fixed inset-0 flex items-center justify-center z-50"
      style={{ background: "var(--modal-backdrop)" }}
      onClick={onClose}
    >
      <div
        className="rounded-lg shadow-2xl w-full max-w-xl mx-4 flex flex-col"
        style={{
          background: "var(--bg-secondary)",
          border: "1px solid var(--border)",
          maxHeight: "80vh",
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 py-4 border-b" style={{ borderColor: "var(--border)" }}>
          <h2 className="text-sm font-semibold" style={{ color: "var(--text-primary)" }}>
            Pick a default model for {provider}
          </h2>
          <p className="text-xs mt-1" style={{ color: "var(--text-secondary)" }}>
            Your API key is saved. Choose the model thClaws should default
            to. You can switch any time with <code className="font-mono">/model</code>.
          </p>
        </div>

        <div className="px-5 py-3 border-b" style={{ borderColor: "var(--border)" }}>
          <input
            ref={inputRef}
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={`Search ${models.length} model${models.length === 1 ? "" : "s"}…`}
            // Disable autocorrect / autocapitalize / spellcheck so model
            // names like "gpt-4.1-nano" or "claude-sonnet-4-6" aren't
            // silently rewritten by the browser's IME helpers.
            autoCorrect="off"
            autoCapitalize="off"
            autoComplete="off"
            spellCheck={false}
            className="w-full px-3 py-2 rounded text-sm outline-none"
            style={{
              background: "var(--bg-tertiary)",
              border: "1px solid var(--border)",
              color: "var(--text-primary)",
            }}
          />
        </div>

        <div className="flex-1 overflow-y-auto py-1">
          {filtered.length === 0 ? (
            <div
              className="px-5 py-4 text-xs text-center"
              style={{ color: "var(--text-secondary)" }}
            >
              No models match "{query}".
            </div>
          ) : (
            filtered.map((m) => {
              const isCurrent = m.id === current;
              const ctx = formatCtx(m.context);
              return (
                <button
                  key={m.id}
                  type="button"
                  onClick={() => pick(m.id)}
                  className="w-full px-5 py-2 text-left text-sm flex items-center justify-between"
                  style={{
                    background: isCurrent ? "var(--bg-tertiary)" : "transparent",
                    color: "var(--text-primary)",
                    cursor: "pointer",
                    borderLeft: isCurrent
                      ? "2px solid var(--accent)"
                      : "2px solid transparent",
                  }}
                  onMouseEnter={(e) =>
                    (e.currentTarget.style.background = "var(--bg-tertiary)")
                  }
                  onMouseLeave={(e) =>
                    (e.currentTarget.style.background = isCurrent
                      ? "var(--bg-tertiary)"
                      : "transparent")
                  }
                >
                  <span className="font-mono truncate">{m.id}</span>
                  {ctx && (
                    <span
                      className="text-xs ml-3 shrink-0"
                      style={{ color: "var(--text-secondary)" }}
                    >
                      {ctx} ctx
                    </span>
                  )}
                </button>
              );
            })
          )}
        </div>

        <div
          className="px-5 py-3 border-t flex items-center justify-between"
          style={{ borderColor: "var(--border)" }}
        >
          <span className="text-xs" style={{ color: "var(--text-secondary)" }}>
            Currently: <code className="font-mono">{current || "(none)"}</code>
          </span>
          <button
            type="button"
            onClick={onClose}
            className="px-3 py-1.5 text-xs rounded"
            style={{
              background: "transparent",
              color: "var(--text-secondary)",
              border: "1px solid var(--border)",
              cursor: "pointer",
            }}
          >
            Skip
          </button>
        </div>
      </div>
    </div>
  );
}
