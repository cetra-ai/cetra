import { Link } from "@tanstack/react-router";
import { Loader2 } from "lucide-react";
import type { ReactNode } from "react";
import { $api } from "@/api/client";
import { projectRef } from "@/api/project-ref";
import type { Schema } from "@/api/types";
import { OutputDiff } from "@/components/experiments/output-diff";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import {
  type DatasetSearch,
  itemShortId,
  prettyJson,
  previewFields,
  sameJson,
} from "@/lib/datasets";

type ItemChange = Schema<"ItemChange">;

/** The paged request of the items one version changed (D-J-24). */
export function versionChangesInit(org: string, project: string, dataset: string, version: number) {
  return {
    params: {
      path: { project: projectRef(org, project), dataset, version },
      query: { limit: 50 },
    },
  };
}

/**
 * The items a version added, edited and deleted, paged by item id. Each change links to the item:
 * a deleted one at the version before (where it still exists), the others in the latest version.
 */
export function VersionChanges({ datasetId, version }: { datasetId: string; version: number }) {
  const { org, project } = useProject();
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/datasets/{dataset}/versions/{version}/changes",
    versionChangesInit(org, project, datasetId, version),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );

  if (query.error) {
    throw query.error;
  }

  const rows = query.data?.pages.flatMap((page) => page.data) ?? [];

  if (query.isPending) {
    return <Skeleton className="h-8 w-full" />;
  }
  if (rows.length === 0) {
    return <p className="text-body text-muted-foreground">No item changed</p>;
  }

  return (
    <div className="flex flex-col gap-2">
      {rows.map((change) => (
        <ChangeRow
          key={`${change.change}-${change.item_id}`}
          datasetId={datasetId}
          change={change}
          version={version}
        />
      ))}
      {query.hasNextPage ? (
        <Button
          type="button"
          variant="outline"
          size="sm"
          className="w-fit text-body"
          disabled={query.isFetchingNextPage}
          onClick={() => {
            void query.fetchNextPage();
          }}
        >
          {query.isFetchingNextPage ? <Loader2 className="animate-spin" /> : null}
          Load more changes
        </Button>
      ) : null}
    </div>
  );
}

/** One changed item: its kind, a link to it and what changed. */
function ChangeRow({
  datasetId,
  change,
  version,
}: {
  datasetId: string;
  change: ItemChange;
  version: number;
}) {
  const { org, project } = useProject();
  const datasetSearch: DatasetSearch =
    change.change === "deleted"
      ? { tab: "items", version: version - 1, item: change.item_id }
      : { tab: "items", item: change.item_id };

  return (
    <div className="flex flex-col gap-1 border-l-2 pl-2 text-body">
      <div className="flex min-w-0 items-center gap-2">
        <span className="shrink-0 text-muted-foreground">{KIND_LABEL[change.change]}</span>
        <Link
          to="/$org/$project/datasets/$datasetId"
          params={{ org, project, datasetId }}
          search={datasetSearch}
          state={true}
          title={change.item_id}
          className="rounded-sm font-mono underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
        >
          {itemShortId(change.item_id)}
        </Link>
      </div>
      {change.change === "added" ? (
        <p className="truncate text-muted-foreground">{previewFields(change.after?.input)}</p>
      ) : null}
      {change.change === "deleted" ? (
        <p className="truncate text-muted-foreground">{previewFields(change.before?.input)}</p>
      ) : null}
      {change.change === "edited" ? (
        <EditDetail before={change.before} after={change.after} />
      ) : null}
    </div>
  );
}

const KIND_LABEL: Record<ItemChange["change"], string> = {
  added: "Added",
  edited: "Edited",
  deleted: "Deleted",
};

/**
 * The fields an edit changed, each as a word diff when both sides have a value, else as its before
 * and after JSON (an expected output added or cleared). Unchanged fields are left out.
 */
function EditDetail({
  before,
  after,
}: {
  before: ItemChange["before"];
  after: ItemChange["after"];
}) {
  const fields = [
    { label: "Input", before: before?.input, after: after?.input },
    { label: "Expected output", before: before?.expected_output, after: after?.expected_output },
    { label: "Metadata", before: before?.metadata, after: after?.metadata },
  ].filter((field) => !sameJson(field.before, field.after));
  if (fields.length === 0) {
    return <p className="text-caption text-muted-foreground">No field changed.</p>;
  }
  return (
    <div className="flex flex-col gap-2">
      {fields.map((field) => (
        <DiffBlock key={field.label} label={field.label}>
          {field.before !== undefined && field.after !== undefined && field.label !== "Metadata" ? (
            <OutputDiff before={field.before} after={field.after} />
          ) : (
            <BeforeAfter before={field.before} after={field.after} />
          )}
        </DiffBlock>
      ))}
    </div>
  );
}

/** Two JSON blocks side by side; an absent side reads "None". */
function BeforeAfter({ before, after }: { before: unknown; after: unknown }) {
  return (
    <div className="grid grid-cols-2 gap-2">
      <div className="flex min-w-0 flex-col gap-0.5">
        <span className="text-caption text-muted-foreground">Before</span>
        <pre className="overflow-auto whitespace-pre-wrap font-mono text-caption text-muted-foreground">
          {before === undefined ? "None" : prettyJson(before)}
        </pre>
      </div>
      <div className="flex min-w-0 flex-col gap-0.5">
        <span className="text-caption text-muted-foreground">After</span>
        <pre className="overflow-auto whitespace-pre-wrap font-mono text-caption">
          {after === undefined ? "None" : prettyJson(after)}
        </pre>
      </div>
    </div>
  );
}

function DiffBlock({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-caption text-muted-foreground">{label}</span>
      {children}
    </div>
  );
}
