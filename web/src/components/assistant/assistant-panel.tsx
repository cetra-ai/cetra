import { useQuery } from "@tanstack/react-query";
import { SquarePen, X } from "lucide-react";
import { assistantStatusQuery } from "@/api/queries-assistant";
import { ConversationMenu, useNewChat } from "@/components/assistant/conversation-menu";
import { Unavailable } from "@/components/assistant/unavailable";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { TooltipProvider } from "@/components/ui/tooltip";
import { setPanelOpen } from "@/lib/assistant/panel";
import { LLM_ADAPTER_LABELS } from "@/lib/llm-adapters";

/**
 * The assistant panel: a chat list, the page's answer area and the connection notice. Mounted
 * inside the project layout, so it needs `ProjectProvider`. Keyed by project by its caller.
 */
export function AssistantPanel() {
  const { org, project } = useProject();
  const status = useQuery(assistantStatusQuery(org, project));
  const { newChat, pending } = useNewChat(org, project);
  const usable = status.data?.usable === true;

  return (
    <TooltipProvider>
      <section
        aria-label="Assistant"
        className="flex h-full min-h-0 flex-col bg-background text-body"
        onKeyDown={(event) => {
          // Esc leaves the panel's controls for the page. It does not close the panel (D-AS-27).
          if (event.key === "Escape") {
            (document.activeElement as HTMLElement | null)?.blur();
            event.stopPropagation();
          }
        }}
      >
        <header className="flex h-10 shrink-0 items-center gap-1 border-b px-2">
          <ConversationMenu org={org} project={project} />
          <div className="flex-1" />
          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            aria-label="New chat"
            disabled={pending}
            onClick={newChat}
          >
            <SquarePen />
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            aria-label="Close the assistant"
            onClick={() => {
              setPanelOpen(false);
            }}
          >
            <X />
          </Button>
        </header>
        <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
          {status.isPending ? (
            <div className="flex flex-col gap-2 p-4">
              <Skeleton className="h-4 w-3/4" />
              <Skeleton className="h-4 w-1/2" />
            </div>
          ) : usable ? (
            <div className="flex flex-1 items-center justify-center p-6 text-center text-muted-foreground">
              Ask about this project's traces, metrics, evaluations and settings.
            </div>
          ) : (
            <Unavailable reason={status.data?.reason} />
          )}
        </div>
        {usable && status.data ? (
          <p className="shrink-0 border-t p-3 text-caption text-muted-foreground">
            Answers use{" "}
            <span className="font-medium text-foreground">
              {status.data.connection_name ?? "an LLM connection"}
            </span>{" "}
            (
            {[
              status.data.adapter ? LLM_ADAPTER_LABELS[status.data.adapter] : undefined,
              status.data.model,
            ]
              .filter((part) => part !== undefined && part !== "")
              .join(", ")}
            ). What the assistant reads is sent to that provider.
          </p>
        ) : null}
      </section>
    </TooltipProvider>
  );
}
