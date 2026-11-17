import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { MoreHorizontal } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { experimentsKey } from "@/api/queries-experiments";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { compareBlockReason, expectedRuns } from "@/lib/experiments";
import { formatRelative } from "@/lib/format";
import { canManageDatasets } from "@/lib/roles";

/** Experiments that can be picked for one comparison (D-J-13). */
const MAX_SELECTED = 4;
/** A running experiment older than this shows its age (CG-J5). */
const STALE_RUNNING_MS = 24 * 60 * 60 * 1000;

export function ExperimentsTable(props: {
  rows: Schema<"Experiment">[];
  /** Selected ids in the order they were picked; the first is the baseline. */
  selected: readonly string[];
  onSelectedChange: (ids: string[]) => void;
  /** Hide the Dataset column (the dataset page's tab). */
  hideDataset?: boolean;
}) {
  const { rows, selected, onSelectedChange, hideDataset } = props;
  const { org, project, data } = useProject();
  const queryClient = useQueryClient();
  const canManage = canManageDatasets(data.role);
  const [deleting, setDeleting] = useState<Schema<"Experiment"> | undefined>();
  const remove = $api.useMutation("delete", "/api/v1/projects/{project}/experiments/{id}");

  function closeDelete() {
    setDeleting(undefined);
    remove.reset();
  }

  function onDelete(experiment: Schema<"Experiment">) {
    remove.mutate(
      { params: { path: { project: projectRef(org, project), id: experiment.id } } },
      {
        onSuccess: () => {
          void queryClient.invalidateQueries({ queryKey: experimentsKey(org, project) });
          onSelectedChange(selected.filter((id) => id !== experiment.id));
          closeDelete();
        },
      },
    );
  }

  return (
    <>
      <Table aria-label="Experiments">
        <TableHeader>
          <TableRow>
            <TableHead>
              <span className="sr-only">Select</span>
            </TableHead>
            <TableHead>Name</TableHead>
            {hideDataset ? null : <TableHead>Dataset</TableHead>}
            <TableHead>Status</TableHead>
            <TableHead>Results</TableHead>
            <TableHead>Created</TableHead>
            <TableHead>
              <span className="sr-only">Actions</span>
            </TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((row) => {
            const isSelected = selected.includes(row.id);
            return (
              <TableRow key={row.id}>
                <TableCell>
                  <Checkbox
                    aria-label={`Select ${row.name}`}
                    checked={isSelected}
                    disabled={!isSelected && selected.length >= MAX_SELECTED}
                    onCheckedChange={(checked) => {
                      if (checked === true) {
                        if (!isSelected) {
                          onSelectedChange([...selected, row.id]);
                        }
                      } else {
                        onSelectedChange(selected.filter((id) => id !== row.id));
                      }
                    }}
                  />
                </TableCell>
                <TableCell>
                  <Link
                    to="/$org/$project/experiments/$experimentId"
                    params={{ org, project, experimentId: row.id }}
                    className="rounded-sm underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
                  >
                    {row.name}
                  </Link>
                </TableCell>
                {hideDataset ? null : (
                  <TableCell>
                    {row.dataset_id !== undefined ? (
                      <Link
                        to="/$org/$project/datasets/$datasetId"
                        params={{ org, project, datasetId: row.dataset_id }}
                        className="rounded-sm underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
                      >
                        {`${row.dataset_name} v${row.dataset_version}`}
                      </Link>
                    ) : (
                      <span>
                        {`${row.dataset_name} v${row.dataset_version} `}
                        <span className="text-muted-foreground">(deleted)</span>
                      </span>
                    )}
                  </TableCell>
                )}
                <TableCell>
                  <StatusBadge experiment={row} />
                </TableCell>
                <TableCell className="tabular-nums">{`${row.result_count} / ${expectedRuns(row)}`}</TableCell>
                <TableCell>
                  <TimeText value={row.created_at} style="datetime" />
                </TableCell>
                <TableCell>
                  <DisabledReason
                    reason={canManage ? undefined : "Only members can delete experiments"}
                  >
                    <DropdownMenu>
                      <DropdownMenuTrigger asChild>
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon-sm"
                          aria-label={`Actions of ${row.name}`}
                          disabled={!canManage}
                        >
                          <MoreHorizontal />
                        </Button>
                      </DropdownMenuTrigger>
                      <DropdownMenuContent align="end">
                        <DropdownMenuItem
                          variant="destructive"
                          onSelect={() => {
                            setDeleting(row);
                          }}
                        >
                          Delete
                        </DropdownMenuItem>
                      </DropdownMenuContent>
                    </DropdownMenu>
                  </DisabledReason>
                </TableCell>
              </TableRow>
            );
          })}
        </TableBody>
      </Table>
      <ConfirmDialog
        open={deleting !== undefined}
        onOpenChange={(open) => {
          if (!open) {
            closeDelete();
          }
        }}
        title="Delete experiment"
        description="Deletes the experiment and its results. Its traces and scores stay in the project."
        confirmLabel="Delete"
        destructive
        pending={remove.isPending}
        error={
          remove.error
            ? problemMessage(remove.error, "The experiment could not be deleted")
            : undefined
        }
        onConfirm={() => {
          if (deleting) {
            onDelete(deleting);
          }
        }}
      />
    </>
  );
}

/**
 * The selection above the table: the baseline (the first picked) and the Compare button, which
 * stays disabled with its reason until the picks can be compared (D-J-13).
 */
export function CompareSelection({ experiments }: { experiments: Schema<"Experiment">[] }) {
  const { org, project } = useProject();
  const navigate = useNavigate();
  const [baseline] = experiments;
  if (baseline === undefined) {
    return null;
  }
  const reason = compareBlockReason(experiments);
  return (
    <div className="flex flex-wrap items-center justify-between gap-3 rounded-md border px-3 py-2 text-body">
      <p>{`Baseline: ${baseline.name}`}</p>
      <DisabledReason reason={reason}>
        <Button
          type="button"
          disabled={reason !== undefined}
          onClick={() => {
            // The ids in pick order, the first is the baseline (D-J-17).
            void navigate({
              to: "/$org/$project/experiments/compare",
              params: { org, project },
              search: { ids: experiments.map((experiment) => experiment.id).join(",") },
            });
          }}
        >
          Compare
        </Button>
      </DisabledReason>
    </div>
  );
}

export function StatusBadge({ experiment }: { experiment: Schema<"Experiment"> }) {
  switch (experiment.status) {
    case "completed":
      return <Badge variant="secondary">Completed</Badge>;
    case "failed": {
      const badge = <Badge variant="destructive">Failed</Badge>;
      if (experiment.error === undefined) {
        return badge;
      }
      return (
        <Tooltip>
          <TooltipTrigger asChild>{badge}</TooltipTrigger>
          <TooltipContent>{experiment.error}</TooltipContent>
        </Tooltip>
      );
    }
    case "running": {
      const started = Date.parse(experiment.created_at);
      const stale = !Number.isNaN(started) && Date.now() - started > STALE_RUNNING_MS;
      return (
        <Badge variant="outline">
          {stale
            ? `Running · started ${formatRelative(experiment.created_at, Date.now())}`
            : "Running"}
        </Badge>
      );
    }
  }
}
