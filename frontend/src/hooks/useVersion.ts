import { useEffect, useState } from "react";
import { subscribe } from "./useIPC";

/// Subscribe to the `initial_state` envelope and pluck the app version
/// (`CARGO_PKG_VERSION`, e.g. "0.9.0") for chrome that wants to surface
/// build info — the Chat empty-state header and the Terminal banner.
///
/// Returns `null` until the first `initial_state` arrives. The hook
/// stays subscribed so a session reload (which re-broadcasts
/// `initial_state`) keeps the value fresh.
export function useVersion(): string | null {
  const [version, setVersion] = useState<string | null>(null);
  useEffect(() => {
    const unsub = subscribe((msg) => {
      if (msg.type === "initial_state" && typeof msg.version === "string") {
        setVersion(msg.version as string);
      }
    });
    return unsub;
  }, []);
  return version;
}
