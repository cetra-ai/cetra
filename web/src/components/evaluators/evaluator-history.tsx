import { History, Loader2 } from "lucide-react";
import { useEffect, useState } from "react";
import { $api } from "@/api/client";
import { evaluatorVersionsInit } from "@/api/queries-evaluation";
import type { Schema } from "@/api/types";
import { CreatedBy } from "@/components/created-by";
import { EmptyState } from "@/components/empty-state";
import { TimeText } from "@/components/time-text";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { type FieldChange, type VersionDiff, versionDiff } from "@/lib/evaluator-diff";
import type { DiffLine } from "@/lib/line-diff";

type Version = Schema<"EvaluatorVersion">;

const WEBHOOK_NOTE =
  "No visible change. A webhook's URL, headers and secret are write-only, so a change to them does not show here.";

/** The version history of an evaluator, newest first, with the diff of the selected version. */
export function EvaluatorHistory({
  org,
  project,
  evaluatorId,
}: {
  org: string;
  project: string;
  evaluatorId: string;
}) {
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/evaluators/{id}/versions",
    evaluatorVersionsInit(org, project, evaluatorId),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  const { fetchNextPage, hasNextPage, isFetchingNextPage } = query;
  const rows = query.data?.pages.flatMap((page) => page.data) ?? [];
  const [selected, setSelected] = useState<number | undefined>();
  const current = rows.find((row) => row.version === selected) ?? rows[0];
  const previous =
    current === undefined ? undefined : rows.find((row) => row.version === current.version - 1);
  // Version 1 has no predecessor, so only later versions need their previous page.
  const needsPrevious = current !== undefined && current.version > 1 && previous === undefined;

  useEffect(() => {
    if (needsPrevious && hasNextPage && !isFetchingNextPage) {
      void fetchNextPage();
    }
  }, [needsPrevious, hasNextPage, isFetchingNextPage, fetchNextPage]);

  if (query.error) {
    throw query.error;
  }
  if (query.isPending) {
    return (
      <div className="flex flex-col gap-2">
        <Skeleton className="h-8 w-full" />
        <Skeleton className="h-8 w-full" />
      </div>
    );
  }
  if (current === undefined) {
    return (
      <EmptyState
        icon={History}
        title="No version history yet"
        description="It starts with the next change to the evaluator's configuration."
      />
    );
  }

  const loadingPrevious = needsPrevious && hasNextPage;
  let subtitle: string;
  if (previous !== undefined) {
    subtitle = `Compared with version ${previous.version}`;
  } else if (loadingPrevious) {
    subtitle = "Loading the previous version…";
  } else {
    subtitle = current.version === 1 ? "First version" : "Earliest recorded version";
  }

  return (
    <div className="grid gap-4 md:grid-cols-[220px_1fr]">
      <div className="flex flex-col gap-3">
        <ul className="flex flex-col gap-2">
          {rows.map((row) => {
            const isCurrent = row.version === current.version;
            return (
              <li key={row.version} className="flex flex-col gap-0.5">
                <button
                  type="button"
                  aria-current={isCurrent ? "true" : undefined}
                  onClick={() => setSelected(row.version)}
                  className={`flex flex-col items-start rounded-md px-2 py-1.5 text-left text-[13px] hover:bg-muted ${isCurrent ? "bg-muted" : ""}`}
                >
                  <span className="font-mono text-[12px]">v{row.version}</span>
                  <TimeText value={row.created_at} style="datetime" />
                </button>
                <div className="px-2 text-[12px] text-muted-foreground">
                  <CreatedBy createdBy={row.created_by} />
                </div>
              </li>
            );
          })}
        </ul>
        {hasNextPage ? (
          <Button
            type="button"
            variant="outline"
            className="w-fit"
            disabled={isFetchingNextPage}
            onClick={() => {
              void fetchNextPage();
            }}
          >
            {isFetchingNextPage ? <Loader2 className="animate-spin" /> : null}
            Load more
          </Button>
        ) : null}
      </div>
      <section className="flex flex-col gap-3">
        <div className="flex flex-col gap-1">
          <h2 className="text-base font-medium">Version {current.version}</h2>
          <p className="text-[12px] text-muted-foreground">{subtitle}</p>
        </div>
        {loadingPrevious ? null : (
          <DiffBody current={current} diff={versionDiff(previous, current)} />
        )}
      </section>
    </div>
  );
}

function DiffBody({ current, diff }: { current: Version; diff: VersionDiff }) {
  if (diff.nothingVisible) {
    return (
      <p className="text-[13px] text-muted-foreground">
        {current.kind === "webhook" ? WEBHOOK_NOTE : "No change."}
      </p>
    );
  }
  const fields = [
    ...diff.config.map((change) => ({ field: change.path, change })),
    ...diff.output.map((change) => ({ field: `output_score.${change.path}`, change })),
  ];
  return (
    <div className="flex flex-col gap-4">
      {diff.name ? <p className="text-[13px]">{nameText(diff.name)}</p> : null}
      {diff.prompt ? <Prompt lines={diff.prompt} /> : null}
      {fields.length > 0 ? (
        <Table aria-label="Changed fields">
          <TableHeader>
            <TableRow>
              <TableHead>Field</TableHead>
              <TableHead>Before</TableHead>
              <TableHead>After</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {fields.map(({ field, change }) => (
              <TableRow key={field}>
                <TableCell className="font-mono text-[12px]">{field}</TableCell>
                <TableCell className="font-mono text-[12px]">
                  {formatValue(change.before)}
                </TableCell>
                <TableCell className="font-mono text-[12px]">{formatValue(change.after)}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      ) : null}
    </div>
  );
}

function Prompt({ lines }: { lines: DiffLine[] }) {
  return (
    <pre className="font-mono text-[12px] whitespace-pre-wrap">
      {lines.map((line, index) => (
        <div
          // The lines of one diff never reorder, so their position is a stable key.
          // biome-ignore lint/suspicious/noArrayIndexKey: a diff is rendered once, in order
          key={index}
          data-op={line.op}
          className={
            line.op === "added"
              ? "bg-emerald-500/10"
              : line.op === "removed"
                ? "bg-red-500/10"
                : "text-muted-foreground"
          }
        >
          {line.op === "added" ? "+ " : line.op === "removed" ? "- " : "  "}
          {line.text}
        </div>
      ))}
    </pre>
  );
}

function nameText(change: FieldChange): string {
  if (change.before === undefined) {
    return `Name: ${formatPlain(change.after)}`;
  }
  return `Name: ${formatPlain(change.before)} -> ${formatPlain(change.after)}`;
}

function formatPlain(value: unknown): string {
  return typeof value === "string" ? value : JSON.stringify(value);
}

function formatValue(value: unknown): string {
  return value === undefined ? "—" : JSON.stringify(value);
}
