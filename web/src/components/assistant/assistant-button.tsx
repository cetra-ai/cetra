import { Sparkles } from "lucide-react";
import { $api } from "@/api/client";
import { projectInit } from "@/api/queries";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { togglePanel, usePanelOpen } from "@/lib/assistant/panel";
import { canUseAssistant } from "@/lib/roles";

/** The top bar's toggle for the assistant panel. Renders nothing for roles below member. */
export function AssistantButton({ org, project }: { org: string; project: string }) {
  const role = $api.useQuery("get", "/api/v1/projects/{project}", projectInit(org, project)).data
    ?.role;
  const open = usePanelOpen();
  if (!canUseAssistant(role)) {
    return null;
  }
  return (
    <TooltipProvider>
      <Tooltip>
        <TooltipTrigger asChild>
          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            aria-label="Assistant"
            aria-pressed={open}
            onClick={togglePanel}
          >
            <Sparkles />
          </Button>
        </TooltipTrigger>
        <TooltipContent>Assistant (⌘I)</TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}
