import { Link, useNavigate } from "@tanstack/react-router";
import type { ReactNode } from "react";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { usePatchIssueSource } from "@/api/queries-issues";
import type { Schema } from "@/api/types";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { RelativeTime } from "@/components/relative-time";
import { Switch } from "@/components/ui/switch";

type Source = Schema<"IssueSource">;

export const CANNOT_MANAGE_SOURCES = "Only members can manage issue sources";

const KIND_LABELS: Record<Schema<"IssueSourceKind">, string> = { error: "Errors" };

/** "1 open issue" or "2 open issues". */
export function openIssuesText(count: number): string {
  return `${count} open ${count === 1 ? "issue" : "issues"}`;
}

/** The source's enabled flag, saved as it is toggled. A plan-limit refusal shows the upgrade. */
function EnabledSourceSwitch({
  org,
  project,
  source,
  canManage,
}: {
  org: string;
  project: string;
  source: Source;
  canManage: boolean;
}) {
  const upgradeUrl = useOrgEntitlements(org).upgradeUrl;
  const update = usePatchIssueSource(org, project);
  const planLimited = problemOf(update.error)?.code === "plan_limit";

  return (
    <div className="flex flex-col items-start gap-1">
      <DisabledReason reason={canManage ? undefined : CANNOT_MANAGE_SOURCES}>
        <Switch
          aria-label={`Enable ${source.name}`}
          checked={source.enabled}
          disabled={!canManage || update.isPending}
          onCheckedChange={(enabled) => {
            update.mutate({
              params: { path: { project: projectRef(org, project), id: source.id } },
              body: { enabled },
            });
          }}
        />
      </DisabledReason>
      {update.error ? (
        <>
          <FormError message={problemMessage(update.error, "Could not update the source.")} />
          {planLimited ? (
            <p className="text-caption">Upgrade your plan or disable another source.</p>
          ) : null}
          {planLimited ? <UpgradeButton url={upgradeUrl} /> : null}
        </>
      ) : null}
    </div>
  );
}

function columnsOf(org: string, project: string, canManage: boolean): DataTableColumn<Source>[] {
  return [
    {
      id: "name",
      size: 240,
      header: "Name",
      enableSorting: false,
      cell: ({ row }) => <span className="truncate font-medium">{row.original.name}</span>,
    },
    {
      id: "kind",
      size: 112,
      header: "Kind",
      enableSorting: false,
      cell: ({ row }) => KIND_LABELS[row.original.kind],
    },
    {
      id: "filter",
      size: 280,
      header: "Filter",
      enableSorting: false,
      cell: ({ row }) =>
        row.original.filter ? (
          <code className="block truncate font-mono text-caption" title={row.original.filter}>
            {row.original.filter}
          </code>
        ) : (
          <span className="text-muted-foreground">No filter</span>
        ),
    },
    {
      id: "enabled",
      size: 176,
      header: "Enabled",
      enableSorting: false,
      cell: ({ row }) => (
        <EnabledSourceSwitch
          org={org}
          project={project}
          source={row.original}
          canManage={canManage}
        />
      ),
    },
    {
      id: "open_issues",
      size: 152,
      header: "Open issues",
      enableSorting: false,
      cell: ({ row }) => (
        <Link
          to="/$org/$project/issues"
          params={{ org, project }}
          search={{ source: row.original.id }}
          className="underline tabular-nums"
        >
          {openIssuesText(row.original.open_issues)}
        </Link>
      ),
    },
    {
      id: "updated",
      size: 176,
      header: "Updated",
      enableSorting: false,
      cell: ({ row }) => <RelativeTime value={row.original.updated_at} />,
    },
  ];
}

/** The project's issue sources. Activating a row opens its edit page. */
export function SourceList({
  org,
  project,
  rows,
  canManage,
  emptyState,
}: {
  org: string;
  project: string;
  rows: Source[];
  canManage: boolean;
  emptyState: ReactNode;
}) {
  const navigate = useNavigate();
  return (
    <DataTable
      tableId="issue-sources"
      ariaLabel="Issue sources"
      columns={columnsOf(org, project, canManage)}
      rows={rows}
      getRowId={(row) => row.id}
      onActivate={(row) => {
        void navigate({
          to: "/$org/$project/issue-sources/$sourceId/edit",
          params: { org, project, sourceId: row.id },
        });
      }}
      hasNextPage={false}
      isFetchingNextPage={false}
      fetchNextPage={() => undefined}
      emptyState={emptyState}
    />
  );
}
