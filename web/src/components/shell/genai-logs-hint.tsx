import { X } from "lucide-react";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { STORAGE_PREFIX } from "@/product";
import { useProject } from "./project-context";

function hintKey(projectRef: string): string {
  return `${STORAGE_PREFIX}genai-logs-hint.${projectRef}`;
}

function wasDismissed(projectRef: string): boolean {
  try {
    return localStorage.getItem(hintKey(projectRef)) !== null;
  } catch {
    return false;
  }
}

function remember(projectRef: string): void {
  try {
    localStorage.setItem(hintKey(projectRef), "1");
  } catch {
    // Private mode or a full quota: the hint comes back on the next visit.
  }
}

/**
 * Says once per project that GenAI content sent as OTLP log events was dropped (D-G-40).
 * The server counts the records; the UI shows the hint when the count is above zero.
 */
export function GenAiLogsHint() {
  const { ref, data } = useProject();
  const [dismissed, setDismissed] = useState(() => wasDismissed(ref));
  const dropped = data.genai_log_events ?? 0;
  if (dropped === 0 || dismissed) {
    return null;
  }
  return (
    <div
      role="status"
      className="flex shrink-0 items-start gap-3 border-b bg-muted px-4 py-2 text-sm"
    >
      <p className="flex-1">
        <strong className="font-medium">GenAI content was sent as log events.</strong> This project
        received {dropped.toLocaleString("en-US")} GenAI log {dropped === 1 ? "record" : "records"},
        and they were dropped: only traces are stored. Switch your instrumentation to record prompts
        and completions on spans (its message-content capture setting), and they will show up in the
        trace view.
      </p>
      <Button
        variant="ghost"
        size="icon-sm"
        aria-label="Dismiss"
        onClick={() => {
          remember(ref);
          setDismissed(true);
        }}
      >
        <X />
      </Button>
    </div>
  );
}
