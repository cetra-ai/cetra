import { CopyButton } from "@/components/copy-button";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { shortId } from "@/lib/format";

/** `shown` replaces the default short form (`shortId`) of `value`. */
export function IdText({ value, kind, shown }: { value: string; kind: string; shown?: string }) {
  return (
    <TooltipProvider>
      <span className="group/id inline-flex items-center gap-0.5">
        <Tooltip>
          <TooltipTrigger asChild>
            <span className="font-mono">{shown ?? shortId(value)}</span>
          </TooltipTrigger>
          <TooltipContent>{value}</TooltipContent>
        </Tooltip>
        <span className="opacity-0 group-focus-within/id:opacity-100 group-hover/id:opacity-100">
          <CopyButton value={value} label={`${kind} ID`} />
        </span>
      </span>
    </TooltipProvider>
  );
}
