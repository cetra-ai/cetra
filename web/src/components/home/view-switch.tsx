import { LayoutGrid, List } from "lucide-react";
import type { ComponentType } from "react";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { type ProjectView, setProjectView, useProjectView } from "@/lib/org-home";

const OPTIONS: readonly { id: ProjectView; label: string; icon: ComponentType }[] = [
  { id: "cards", label: "Cards", icon: LayoutGrid },
  { id: "table", label: "Table", icon: List },
];

/** The org home's Cards/Table switch (D-C-27), remembered in the browser. */
export function ProjectViewSwitch() {
  const view = useProjectView();
  return (
    <div
      role="radiogroup"
      aria-label="View"
      className="inline-flex shrink-0 items-center rounded-md border"
    >
      {OPTIONS.map(({ id, label, icon: Icon }) => (
        <Tooltip key={id}>
          <TooltipTrigger asChild>
            <Button
              type="button"
              variant="ghost"
              size="icon-xs"
              role="radio"
              aria-checked={view === id}
              aria-label={label}
              className={view === id ? "bg-muted" : undefined}
              onClick={() => {
                setProjectView(id);
              }}
            >
              <Icon />
            </Button>
          </TooltipTrigger>
          <TooltipContent>{label}</TooltipContent>
        </Tooltip>
      ))}
    </div>
  );
}
