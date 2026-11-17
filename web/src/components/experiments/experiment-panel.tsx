import { useQuery } from "@tanstack/react-query";
import { X } from "lucide-react";
import type { ReactNode } from "react";
import { problemOf } from "@/api/problem";
import { experimentQuery } from "@/api/queries-experiments";
import type { Schema } from "@/api/types";
import { ExperimentDetail } from "@/components/experiments/experiment-detail";
import { StatusBadge } from "@/components/experiments/experiments-table";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";

/**
 * The experiment drawer beside the list: a header with the name and status, then the experiment
 * in full. `layout` is the list's layout controls. `row` is the experiment as the list has it,
 * shown until the fetch answers. The errors switch and the open item (`errors` and `item` in the
 * list's URL) are the list's state, passed in with their setters.
 */
export function ExperimentPanel({
  experimentId,
  row,
  layout,
  errorsOnly,
  onErrorsOnlyChange,
  item,
  onItemChange,
  onClose,
}: {
  experimentId: string;
  row?: Schema<"Experiment">;
  layout?: ReactNode;
  errorsOnly: boolean;
  onErrorsOnlyChange: (errorsOnly: boolean) => void;
  item: string | undefined;
  onItemChange: (item: string | undefined) => void;
  onClose: () => void;
}) {
  const { org, project } = useProject();
  const query = useQuery(experimentQuery(org, project, experimentId));
  // An experiment the list holds shows at once; once the fetch answers, its answer wins.
  const experiment = query.data ?? (query.isPending ? row : undefined);
  const missing = problemOf(query.error)?.status === 404;

  return (
    <section aria-label="Experiment" className="flex h-full min-h-0 flex-col">
      <header className="flex shrink-0 items-center gap-2 border-b px-3 py-1.5 text-body">
        {experiment ? (
          <span className="flex min-w-0 items-center gap-2">
            <span className="truncate text-ui font-semibold">{experiment.name}</span>
            <StatusBadge experiment={experiment} />
          </span>
        ) : null}
        <div className="ml-auto flex shrink-0 items-center gap-1">
          {layout}
          <Button type="button" variant="ghost" size="icon-xs" aria-label="Close" onClick={onClose}>
            <X />
          </Button>
        </div>
      </header>
      <div className="min-h-0 flex-1 overflow-auto p-3">
        {experiment ? (
          <ExperimentDetail
            key={experimentId}
            experimentId={experimentId}
            compact
            errorsOnly={errorsOnly}
            onErrorsOnlyChange={onErrorsOnlyChange}
            item={item}
            onItemChange={onItemChange}
          />
        ) : missing ? (
          <p className="text-muted-foreground">This experiment no longer exists.</p>
        ) : query.isError ? (
          <p className="text-destructive">
            {problemOf(query.error)?.title ?? "Could not load this experiment."}
          </p>
        ) : (
          <div className="flex flex-col gap-2">
            <Skeleton className="h-6 w-full" />
            <Skeleton className="h-6 w-full" />
            <Skeleton className="h-6 w-full" />
          </div>
        )}
      </div>
    </section>
  );
}
