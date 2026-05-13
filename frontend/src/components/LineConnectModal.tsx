import { useEffect, useState } from "react";
import { X, MessageCircle, CheckCircle2, AlertCircle } from "lucide-react";
import { send, subscribe } from "../hooks/useIPC";

/// Pair-then-status modal for the LINE bridge (plan-07 Phase 1.3).
///
/// Two states:
/// - **Disconnected** — pairing-code input + Connect button.
///   Submits to `line_pair`, which round-trips POST /pair on the
///   relay and (on success) saves the binding token and signals
///   the worker to spawn the WS session.
/// - **Connected** — server URL + Disconnect button. Disconnect
///   sends `line_disconnect`, which cancels the worker's WS task
///   and deletes the on-disk config.
///
/// `chat_line_status` envelopes flow in via subscribe() so the
/// modal stays in sync with the worker even when the user paired
/// from a different surface (e.g. CLI flag in a future Phase).

type Status = {
  state: "connected" | "disconnected";
  server_url: string;
  pending_approvals: number;
};

export function LineConnectModal({ onClose }: { onClose: () => void }) {
  const [status, setStatus] = useState<Status>({
    state: "disconnected",
    server_url: "",
    pending_approvals: 0,
  });
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "line_status") {
        setStatus({
          state: (msg.state as Status["state"]) ?? "disconnected",
          server_url: (msg.server_url as string) ?? "",
          pending_approvals: (msg.pending_approvals as number) ?? 0,
        });
      } else if (msg.type === "line_pair_result") {
        setBusy(false);
        if (msg.ok) {
          // status will update via the worker's broadcast — clear
          // the input and error.
          setCode("");
          setError(null);
        } else {
          setError((msg.error as string) ?? "pairing failed");
        }
      } else if (msg.type === "line_disconnect_ack") {
        setBusy(false);
      }
    });
    // Ask for the current status on mount so the modal opens with
    // the right view (Disconnected vs Connected).
    send({ type: "line_status" });
    return unsub;
  }, []);

  // ESC closes.
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [onClose]);

  const handleConnect = () => {
    const trimmed = code.trim().toUpperCase();
    if (trimmed.length === 0) return;
    setError(null);
    setBusy(true);
    send({ type: "line_pair", code: trimmed });
  };

  const handleDisconnect = () => {
    setBusy(true);
    send({ type: "line_disconnect" });
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center"
      style={{ background: "rgba(0,0,0,0.5)" }}
      onClick={onClose}
    >
      <div
        className="rounded-lg shadow-2xl"
        style={{
          background: "var(--bg-primary)",
          border: "1px solid var(--border)",
          width: "440px",
          maxWidth: "90vw",
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <div
          className="flex items-center justify-between px-4 py-3 border-b"
          style={{ borderColor: "var(--border)" }}
        >
          <div className="flex items-center gap-2">
            <MessageCircle size={16} style={{ color: "var(--accent)" }} />
            <span
              className="font-semibold text-sm"
              style={{ color: "var(--text-primary)" }}
            >
              Line Connect
            </span>
          </div>
          <button
            onClick={onClose}
            className="p-1 rounded hover:bg-white/10"
            style={{ color: "var(--text-secondary)" }}
            title="Close (Esc)"
          >
            <X size={14} />
          </button>
        </div>

        <div className="px-4 py-4 space-y-4">
          {status.state === "connected" ? (
            <ConnectedView
              status={status}
              busy={busy}
              onDisconnect={handleDisconnect}
            />
          ) : (
            <DisconnectedView
              code={code}
              setCode={setCode}
              busy={busy}
              error={error}
              onConnect={handleConnect}
            />
          )}
        </div>
      </div>
    </div>
  );
}

function DisconnectedView({
  code,
  setCode,
  busy,
  error,
  onConnect,
}: {
  code: string;
  setCode: (s: string) => void;
  busy: boolean;
  error: string | null;
  onConnect: () => void;
}) {
  return (
    <>
      <p className="text-xs" style={{ color: "var(--text-secondary)" }}>
        Send any message to your thClaws LINE OA, then paste the 8-character
        pairing code below. The bridge runs in the background once paired —
        agent stays on this machine; LINE is just the chat surface.
      </p>
      <div className="space-y-2">
        <label
          className="block text-xs font-semibold"
          style={{ color: "var(--text-primary)" }}
        >
          Pairing code
        </label>
        <input
          type="text"
          value={code}
          onChange={(e) => setCode(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") onConnect();
          }}
          placeholder="ABCD1234"
          maxLength={8}
          className="w-full px-3 py-2 rounded font-mono text-sm tracking-wider uppercase"
          style={{
            background: "var(--bg-secondary)",
            border: "1px solid var(--border)",
            color: "var(--text-primary)",
          }}
          autoFocus
        />
      </div>
      {error && (
        <div
          className="flex items-start gap-2 text-xs px-3 py-2 rounded"
          style={{
            background: "var(--bg-secondary)",
            color: "var(--danger, #e06c75)",
            border: "1px solid var(--border)",
          }}
        >
          <AlertCircle size={14} className="shrink-0 mt-0.5" />
          <span>{error}</span>
        </div>
      )}
      <div className="flex justify-end gap-2">
        <button
          onClick={onConnect}
          disabled={busy || code.trim().length === 0}
          className="px-3 py-1.5 rounded text-xs font-semibold"
          style={{
            background:
              busy || code.trim().length === 0
                ? "var(--bg-secondary)"
                : "var(--accent)",
            color: "var(--accent-fg, #ffffff)",
            opacity: busy || code.trim().length === 0 ? 0.5 : 1,
          }}
        >
          {busy ? "Connecting…" : "Connect"}
        </button>
      </div>
    </>
  );
}

function ConnectedView({
  status,
  busy,
  onDisconnect,
}: {
  status: Status;
  busy: boolean;
  onDisconnect: () => void;
}) {
  return (
    <>
      <div
        className="flex items-start gap-2 text-xs px-3 py-2 rounded"
        style={{
          background: "var(--bg-secondary)",
          border: "1px solid var(--border)",
        }}
      >
        <CheckCircle2
          size={14}
          className="shrink-0 mt-0.5"
          style={{ color: "var(--success, #98c379)" }}
        />
        <div className="space-y-1">
          <div style={{ color: "var(--text-primary)" }}>
            <strong>Connected.</strong> Send a message to your LINE OA to
            verify end-to-end.
          </div>
          {status.server_url && (
            <div
              className="font-mono"
              style={{ color: "var(--text-secondary)", fontSize: "10px" }}
            >
              {status.server_url}
            </div>
          )}
        </div>
      </div>
      <div className="flex justify-end">
        <button
          onClick={onDisconnect}
          disabled={busy}
          className="px-3 py-1.5 rounded text-xs font-semibold"
          style={{
            background: "var(--bg-secondary)",
            border: "1px solid var(--border)",
            color: "var(--danger, #e06c75)",
            opacity: busy ? 0.5 : 1,
          }}
        >
          {busy ? "Disconnecting…" : "Disconnect"}
        </button>
      </div>
    </>
  );
}
