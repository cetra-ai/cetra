import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import {
  Circle,
  History,
  Loader2,
  type LucideIcon,
  Pencil,
  Plus,
  Trash2,
  Upload,
  Waypoints,
} from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { datasetQuery, datasetVersionsInit, datasetWriteKeys } from "@/api/queries-datasets";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { CreatedBy } from "@/components/created-by";
import { VersionChanges } from "@/components/datasets/version-changes";
import { EmptyState } from "@/components/empty-state";
import { RelativeTime } from "@/components/relative-time";
import { useProject } from "@/components/shell/project-context";
import { useRowHeight } from "@/components/theme-provider";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";

type DatasetVersion = Schema<"DatasetVersion">;

/**
 * The version history of a dataset, newest first. A version opens the items of that version. On the
 * latest version, `editable` (a member viewing the latest version) lets an earlier version be restored.
 */
export function VersionsTable({ datasetId, editable }: { datasetId: string; editable: boolean }) {
  const { org, project } = useProject();
  const rowHeight = useRowHeight();
  // The current version's link omits `version`, so the page keeps following the latest version.
  const latest = useQuery(datasetQuery(org, project, datasetId)).data?.current_version;
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/datasets/{dataset}/versions",
    datasetVersionsInit(org, project, datasetId),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  // The versions whose changes are open; a row opens its changes on request.
  const [openChanges, setOpenChanges] = useState<ReadonlySet<number>>(() => new Set());
  // The version a restore is confirmed for, if any.
  const [restoring, setRestoring] = useState<number | undefined>(undefined);
  const restore = $api.useMutation("post", "/api/v1/projects/{project}/datasets/{dataset}/restore");
  const queryClient = useQueryClient();
  const navigate = useNavigate();

  if (query.error) {
    throw query.error;
  }

  const rows = query.data?.pages.flatMap((page) => page.data) ?? [];

  if (query.isPending) {
    return (
      <div className="flex flex-col gap-2">
        <Skeleton className="h-8 w-full" />
        <Skeleton className="h-8 w-full" />
      </div>
    );
  }
  if (rows.length === 0) {
    return <EmptyState icon={History} title="No versions yet" />;
  }

  function toggleChanges(version: number) {
    setOpenChanges((current) => {
      const next = new Set(current);
      if (next.has(version)) {
        next.delete(version);
      } else {
        next.add(version);
      }
      return next;
    });
  }

  function onRestore() {
    if (restoring === undefined) {
      return;
    }
    restore.mutate(
      {
        params: { path: { project: projectRef(org, project), dataset: datasetId } },
        body: { version: restoring },
      },
      {
        onSuccess: () => {
          for (const key of datasetWriteKeys(org, project, datasetId)) {
            void queryClient.invalidateQueries({ queryKey: key });
          }
          setRestoring(undefined);
          void navigate({
            to: "/$org/$project/datasets/$datasetId",
            params: { org, project, datasetId },
            search: { tab: "items" },
          });
        },
      },
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <ol aria-label="Versions" className="flex flex-col">
        {rows.map((version) => {
          const Icon = versionIcon(version.description);
          const isLatest = version.version === latest;
          const total = version.added + version.edited + version.deleted;
          // Version 0 (the dataset's creation) has no changes to list.
          const expandable = version.version >= 1 && total > 0;
          const expanded = expandable && openChanges.has(version.version);
          return (
            <li key={version.version} className="border-b text-body last:border-b-0">
              <div className="flex items-center gap-2" style={{ height: rowHeight }}>
                <Icon className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />
                <span className="w-10 shrink-0 font-mono">{`v${version.version}`}</span>
                <span className="min-w-0 flex-1 truncate" title={version.description}>
                  {version.description}
                </span>
                <ChangeCounts version={version} />
                <span className="shrink-0">
                  <CreatedBy createdBy={version.created_by} />
                </span>
                <span className="shrink-0 text-muted-foreground">
                  <RelativeTime value={version.created_at} />
                </span>
                {expandable ? (
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    className="shrink-0 text-body"
                    aria-expanded={expanded}
                    aria-controls={`version-changes-${version.version}`}
                    onClick={() => {
                      toggleChanges(version.version);
                    }}
                  >
                    Changes
                  </Button>
                ) : null}
                {isLatest ? (
                  <Badge variant="secondary" className="shrink-0 text-caption">
                    latest
                  </Badge>
                ) : (
                  <>
                    {editable && latest !== undefined ? (
                      <Button
                        type="button"
                        variant="ghost"
                        size="sm"
                        className="shrink-0 text-body"
                        onClick={() => {
                          restore.reset();
                          setRestoring(version.version);
                        }}
                      >
                        Restore this version
                      </Button>
                    ) : null}
                    <Button asChild variant="ghost" size="sm" className="shrink-0 text-body">
                      <Link
                        to="/$org/$project/datasets/$datasetId"
                        params={{ org, project, datasetId }}
                        search={{ tab: "items", version: version.version }}
                      >
                        View
                      </Link>
                    </Button>
                  </>
                )}
              </div>
              {expanded ? (
                <div id={`version-changes-${version.version}`} className="pb-2 pl-6 pt-1">
                  <VersionChanges datasetId={datasetId} version={version.version} />
                </div>
              ) : null}
            </li>
          );
        })}
      </ol>
      {query.hasNextPage ? (
        <Button
          type="button"
          variant="outline"
          className="w-fit"
          disabled={query.isFetchingNextPage}
          onClick={() => {
            void query.fetchNextPage();
          }}
        >
          {query.isFetchingNextPage ? <Loader2 className="animate-spin" /> : null}
          Load more
        </Button>
      ) : null}
      {restoring !== undefined && latest !== undefined ? (
        <ConfirmDialog
          open
          onOpenChange={(open) => {
            if (!open) {
              setRestoring(undefined);
              restore.reset();
            }
          }}
          title={`Restore version ${restoring}?`}
          description={`This creates version ${latest + 1} with the items of version ${restoring}.`}
          confirmLabel="Restore"
          pending={restore.isPending}
          error={
            restore.error
              ? (fieldErrors(restore.error).version ??
                problemMessage(restore.error, "The version could not be restored"))
              : undefined
          }
          onConfirm={onRestore}
        />
      ) : null}
    </div>
  );
}

/** The item counts of a version as `+added`, `~edited` and `−deleted`; zero counts are left out. */
function ChangeCounts({ version }: { version: DatasetVersion }) {
  const counts = [
    { sign: "+", count: version.added, label: "added" },
    { sign: "~", count: version.edited, label: "edited" },
    { sign: "−", count: version.deleted, label: "deleted" },
  ].filter((entry) => entry.count > 0);
  if (counts.length === 0) {
    return null;
  }
  return (
    <span className="flex shrink-0 items-center gap-1">
      {counts.map((entry) => (
        <Badge
          key={entry.label}
          variant="outline"
          className="font-mono text-caption text-muted-foreground"
        >
          <span title={`${entry.count} ${entry.label}`}>{`${entry.sign}${entry.count}`}</span>
        </Badge>
      ))}
    </span>
  );
}

/**
 * The icon of a version, from the first word of its description: `add from trace` → Waypoints,
 * `add` → Plus, `edit` → Pencil, `delete` → Trash2, `import` → Upload; anything else → Circle.
 */
function versionIcon(description: string): LucideIcon {
  const text = description.trim().toLowerCase();
  if (text === "add from trace" || text.startsWith("add from trace ")) {
    return Waypoints;
  }
  const word = text.split(/\s+/)[0] ?? "";
  switch (word) {
    case "add":
      return Plus;
    case "edit":
      return Pencil;
    case "delete":
      return Trash2;
    case "import":
      return Upload;
    default:
      return Circle;
  }
}
