import { Maximize2, Minimize2, PanelBottom, PanelRight, PanelTop } from "lucide-react";
import type { ComponentType } from "react";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import {
  type ListLayout,
  setListLayout,
  setTraceMaximized,
  useListLayout,
  useTraceMaximized,
} from "@/lib/list-layout";

const OPTIONS: readonly { id: ListLayout; label: string; icon: ComponentType }[] = [
  { id: "drawer", label: "Drawer", icon: PanelRight },
  { id: "bottom", label: "Bottom", icon: PanelBottom },
  { id: "page", label: "Page", icon: PanelTop },
];

export function LayoutSwitcher() {
  const layout = useListLayout();
  return (
    <div
      role="radiogroup"
      aria-label="Layout"
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
              aria-checked={layout === id}
              aria-label={label}
              className={layout === id ? "bg-muted" : undefined}
              onClick={() => {
                setListLayout(id);
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

/** Shown in the open trace's header: the layout switcher and the maximize toggle. */
export function TraceLayoutControls() {
  const maximized = useTraceMaximized();
  const label = maximized ? "Restore trace" : "Maximize trace";
  return (
    <>
      <LayoutSwitcher />
      <Tooltip>
        <TooltipTrigger asChild>
          <Button
            type="button"
            variant="ghost"
            size="icon-xs"
            aria-label={label}
            aria-pressed={maximized}
            onClick={() => {
              setTraceMaximized(!maximized);
            }}
          >
            {maximized ? <Minimize2 /> : <Maximize2 />}
          </Button>
        </TooltipTrigger>
        <TooltipContent>{label}</TooltipContent>
      </Tooltip>
    </>
  );
}
