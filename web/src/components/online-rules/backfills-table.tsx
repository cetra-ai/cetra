import { useQuery } from "@tanstack/react-query";
import { Loader2 } from "lucide-react";
import { backfillsQuery } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { TimeText } from "@/components/time-text";
import { Badge } from "@/components/ui/badge";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";

type Backfill = Schema<"Backfill">;
type BackfillPage = Schema<"BackfillPage">;

const STATUS_LABELS: Record<Backfill["status"], string> = {
  running: "Running",
  done: "Done",
  failed: "Failed",
};

/** The sentences the stored error strings stand for (spec 06, D-I-41). */
const ERROR_TEXT: Record<string, string> = {
  interrupted: "Interrupted by a server restart. Run it again.",
  "rule disabled": "The rule was disabled.",
  "storage unavailable": "Storage was busy. Run it again.",
  "invalid filter": "The rule's filter no longer parses. Fix the rule, then run it again.",
  "storage read failed": "The traces could not be read.",
  "rule could not be read": "The rule could not be read.",
  "evaluator could not be read": "The evaluator could not be read.",
};

/** Refetch every 2 s while a backfill runs, else not at all. */
export function backfillRefetchInterval(data: BackfillPage | undefined): number | false {
  return data?.data.some((backfill) => backfill.status === "running") ? 2000 : false;
}

/** The rule's backfills, newest first, refreshed while one of them runs. */
export function BackfillsTable({
  org,
  project,
  ruleId,
}: {
  org: string;
  project: string;
  ruleId: string;
}) {
  const query = useQuery({
    ...backfillsQuery(org, project, ruleId),
    refetchInterval: (state) => backfillRefetchInterval(state.state.data),
  });
  if (query.isError) {
    return (
      <p role="alert" className="text-xs text-destructive">
        The backfills could not be loaded.
      </p>
    );
  }
  if (query.isPending) {
    return <p className="text-xs text-muted-foreground">Loading backfills…</p>;
  }
  const backfills = query.data.data;
  if (backfills.length === 0) {
    return <p className="text-xs text-muted-foreground">No backfills yet.</p>;
  }
  return (
    <Table aria-label="Backfills">
      <TableHeader>
        <TableRow>
          <TableHead>Started</TableHead>
          <TableHead>Window</TableHead>
          <TableHead>Status</TableHead>
          <TableHead>Matched</TableHead>
          <TableHead>Queued</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {backfills.map((backfill) => (
          <TableRow key={backfill.id}>
            <TableCell>
              <TimeText value={backfill.created_at} style="datetime" />
            </TableCell>
            <TableCell>
              <span className="inline-flex flex-wrap items-center gap-1">
                <TimeText value={backfill.from} style="datetime" />
                <span aria-hidden="true">–</span>
                <TimeText value={backfill.to} style="datetime" />
              </span>
            </TableCell>
            <TableCell>
              <div className="flex max-w-64 flex-col gap-0.5">
                <Badge variant="outline" className="w-fit">
                  {backfill.status === "running" ? (
                    <Loader2 className="animate-spin" aria-hidden="true" />
                  ) : null}
                  {STATUS_LABELS[backfill.status]}
                </Badge>
                {backfill.status === "failed" && backfill.error ? (
                  <span className="text-xs text-muted-foreground">
                    {ERROR_TEXT[backfill.error] ?? backfill.error}
                  </span>
                ) : null}
              </div>
            </TableCell>
            <TableCell className="tabular-nums">
              {backfill.matched.toLocaleString("en-US")}
            </TableCell>
            <TableCell className="tabular-nums">
              {backfill.enqueued.toLocaleString("en-US")}
            </TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}
