import type { ReactNode } from "react";
import type { Schema } from "@/api/types";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { IssueStateBadge } from "@/components/issues/issue-state-badge";
import { RelativeTime } from "@/components/relative-time";
import { Badge } from "@/components/ui/badge";

type Issue = Schema<"Issue">;

/** Occurrences and users of the period, with the singular for one. */
function activityText(issue: Issue): string {
  const occurrences = `${issue.count} ${issue.count === 1 ? "occurrence" : "occurrences"}`;
  const users = `${issue.users} ${issue.users === 1 ? "user" : "users"}`;
  return `${occurrences} · ${users}`;
}

const COLUMNS: DataTableColumn<Issue>[] = [
  {
    id: "title",
    size: 360,
    header: "Title",
    enableSorting: false,
    cell: ({ row }) => (
      <span className="flex min-w-0 items-center gap-2">
        <span className="truncate font-medium">{row.original.title}</span>
        {row.original.regressed ? <Badge variant="destructive">Regressed</Badge> : null}
      </span>
    ),
  },
  {
    id: "source",
    size: 140,
    header: "Source",
    enableSorting: false,
    cell: ({ row }) => row.original.source_name,
  },
  {
    id: "state",
    size: 128,
    header: "State",
    enableSorting: false,
    cell: ({ row }) => <IssueStateBadge state={row.original.state} />,
  },
  {
    id: "activity",
    size: 224,
    header: "Last 24 h",
    enableSorting: false,
    cell: ({ row }) => activityText(row.original),
  },
  {
    id: "last_seen",
    size: 176,
    header: "Last seen",
    enableSorting: false,
    cell: ({ row }) => <RelativeTime value={row.original.last_seen} />,
  },
  {
    id: "first_seen",
    size: 176,
    header: "First seen",
    enableSorting: false,
    cell: ({ row }) => <RelativeTime value={row.original.first_seen} />,
  },
];

/** The issues of the Issues list, as a keyboard-navigable grid (j/k, Enter opens the issue). */
export function IssueList({
  rows,
  hasNextPage,
  isFetchingNextPage,
  fetchNextPage,
  onOpen,
  emptyState,
}: {
  rows: Issue[];
  hasNextPage: boolean;
  isFetchingNextPage: boolean;
  fetchNextPage: () => void;
  onOpen: (issueId: string) => void;
  emptyState: ReactNode;
}) {
  return (
    <DataTable
      tableId="issues"
      ariaLabel="Issues"
      columns={COLUMNS}
      rows={rows}
      getRowId={(row) => row.id}
      onActivate={(row) => {
        onOpen(row.id);
      }}
      hasNextPage={hasNextPage}
      isFetchingNextPage={isFetchingNextPage}
      fetchNextPage={fetchNextPage}
      emptyState={emptyState}
    />
  );
}
