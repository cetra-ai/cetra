import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { cn } from "cn";
import { useEffect, useState } from "react";
import { projectQuery } from "@/api/queries";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { formatRelative } from "@/lib/format";

const RECEIVING_WINDOW_MS = 5 * 60_000;

/**
 * Whether the project has received spans over OTLP lately (D-C-20). Green while its last batch is
 * under five minutes old, grey when older, and grey with no time when the server has not received
 * one since it started. Renders nothing until the project first loads; a failed refetch keeps the
 * last known state.
 */
export function IngestDot({ org, project }: { org: string; project: string }) {
  const query = useQuery({ ...projectQuery(org, project), refetchInterval: 30_000 });
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 15_000);
    return () => clearInterval(timer);
  }, []);

  if (!query.data) {
    return null;
  }
  const at = query.data.last_ingest_at;
  let receiving = false;
  let text = "No spans received since the server started";
  if (at !== undefined) {
    receiving = now - Date.parse(at) < RECEIVING_WINDOW_MS;
    const ago = formatRelative(at, now);
    text = receiving
      ? `Receiving spans · last ${ago}`
      : `No spans in the last 5 minutes · last ${ago}`;
  }
  // The bar sits outside the sidebar's tooltip provider, so this one has its own.
  return (
    <TooltipProvider>
      <Tooltip>
        <TooltipTrigger asChild>
          <Link
            to="/$org/$project/traces"
            params={{ org, project }}
            aria-label={text}
            className="flex size-6 items-center justify-center rounded-sm outline-none focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
          >
            <span
              className={cn(
                "size-2 rounded-full",
                receiving ? "bg-emerald-500" : "bg-muted-foreground/40",
              )}
            />
          </Link>
        </TooltipTrigger>
        <TooltipContent>{text}</TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}
