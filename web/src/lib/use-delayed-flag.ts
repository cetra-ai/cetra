import { useEffect, useState } from "react";

/**
 * True only once `active` has stayed true for `delayMs`. It turns false at once when `active`
 * becomes false, so a short wait never shows a flash.
 */
export function useDelayedFlag(active: boolean, delayMs = 200): boolean {
  const [elapsed, setElapsed] = useState(false);
  useEffect(() => {
    if (!active) {
      setElapsed(false);
      return;
    }
    const timer = window.setTimeout(() => setElapsed(true), delayMs);
    return () => window.clearTimeout(timer);
  }, [active, delayMs]);
  return active && elapsed;
}
