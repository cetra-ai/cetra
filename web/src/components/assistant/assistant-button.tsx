import { Sparkles } from "lucide-react";
import { $api } from "@/api/client";
import { projectInit } from "@/api/queries";
import { Button } from "@/components/ui/button";
import { togglePanel, usePanelOpen } from "@/lib/assistant/panel";
import { canUseAssistant } from "@/lib/roles";
import { shortcutLabel } from "@/lib/shortcuts";

/**
 * The top bar's toggle for the assistant panel: "Ask AI" and its shortcut, filled while the panel
 * is open. Below `sm` only the icon shows. Renders nothing for roles below member.
 */
export function AssistantButton({ org, project }: { org: string; project: string }) {
  const role = $api.useQuery("get", "/api/v1/projects/{project}", projectInit(org, project)).data
    ?.role;
  const open = usePanelOpen();
  if (!canUseAssistant(role)) {
    return null;
  }
  return (
    <Button
      type="button"
      variant="outline"
      className="h-7 gap-1.5 px-2 text-caption aria-pressed:bg-accent aria-pressed:text-accent-foreground"
      aria-label="Ask AI"
      aria-pressed={open}
      aria-keyshortcuts="Meta+I Control+I"
      onClick={togglePanel}
    >
      <Sparkles aria-hidden="true" className="text-violet-500" />
      <span className="hidden sm:inline">Ask AI</span>
      <kbd className="hidden font-mono text-caption text-muted-foreground md:inline">
        {shortcutLabel("⌘I", navigator.userAgent)}
      </kbd>
    </Button>
  );
}
