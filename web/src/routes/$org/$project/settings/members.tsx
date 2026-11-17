import { createFileRoute } from "@tanstack/react-router";
import { Users } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { $api } from "@/api/client";
import { projectMembersInit } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { EmptyState } from "@/components/empty-state";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import {
  filterRows,
  SettingsListLayout,
  SettingsListToolbar,
  SettingsPanel,
  SettingsTableFrame,
  type SortValue,
  sortRows,
} from "@/components/settings/settings-list";
import { SettingsPage } from "@/components/settings/settings-page";
import { useProject } from "@/components/shell/project-context";
import { Skeleton } from "@/components/ui/skeleton";
import { MemberAccessPanel } from "@/ee/project-roles/member-access-panel";
import { atLeast, canHaveOverride, overrideLabel, roleLabel, roleRank } from "@/lib/roles";
import { parseSettingsListSearch } from "@/lib/search/settings-list";

type Member = Schema<"ProjectMember">;

export const Route = createFileRoute("/$org/$project/settings/members")({
  validateSearch: (search) => parseSettingsListSearch(search, ["name", "effective_role"]),
  component: ProjectMembersPage,
});

function accessText(member: Member): string {
  if (!canHaveOverride(member.org_role)) {
    return "Always admin";
  }
  return member.override ? overrideLabel(member.override) : overrideLabel("inherit");
}

const COLUMNS: DataTableColumn<Member>[] = [
  {
    id: "name",
    size: 280,
    header: "Member",
    enableSorting: true,
    cell: ({ row }) => (
      <div className="flex min-w-0 flex-col">
        <span className="truncate">{row.original.user.name ?? row.original.user.email}</span>
        {row.original.user.name ? (
          <span className="truncate text-caption text-muted-foreground">
            {row.original.user.email}
          </span>
        ) : null}
      </div>
    ),
  },
  {
    id: "org_role",
    size: 120,
    header: "Org role",
    enableSorting: false,
    cell: ({ row }) => roleLabel(row.original.org_role),
  },
  {
    id: "override",
    size: 180,
    header: "Project access",
    enableSorting: false,
    cell: ({ row }) => accessText(row.original),
  },
  {
    id: "effective_role",
    size: 140,
    header: "Effective role",
    enableSorting: true,
    cell: ({ row }) =>
      row.original.effective_role ? roleLabel(row.original.effective_role) : "No access",
  },
];

const SORT_KEYS: Record<string, (member: Member) => SortValue> = {
  name: (member) => (member.user.name ?? member.user.email).toLowerCase(),
  effective_role: (member) => (member.effective_role ? roleRank(member.effective_role) : undefined),
};

function ProjectMembersPage() {
  const { org, project, ref, data } = useProject();
  const canEdit = atLeast(data.role, "admin");
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  const entitlements = useOrgEntitlements(org);
  const locked = entitlements.locked("project_roles");
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const [text, setText] = useState(search.q ?? "");
  // The search text follows the URL when it changes from elsewhere (back, another link).
  useEffect(() => {
    setText((current) => (current.trim() === (search.q ?? "") ? current : (search.q ?? "")));
  }, [search.q]);
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/members",
    projectMembersInit(org, project),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  const fetchNextPage = useCallback(() => {
    void query.fetchNextPage();
  }, [query.fetchNextPage]);

  if (query.error) {
    throw query.error;
  }

  const rows = query.data?.pages.flatMap((page) => page.data) ?? [];
  const sorting = search.sort ? { id: search.sort, desc: search.desc === true } : undefined;
  const visible = sortRows(
    filterRows(rows, search.q, (member) => [member.user.name, member.user.email]),
    sorting,
    SORT_KEYS,
  );
  const openMember =
    search.open === undefined ? undefined : rows.find((member) => member.user.id === search.open);

  function go(
    next: (prev: typeof search) => typeof search,
    options: { replace?: boolean; ignoreBlocker?: boolean } = {},
  ) {
    void navigate({
      to: "/$org/$project/settings/members",
      params: { org, project },
      search: next,
      replace: options.replace,
      ignoreBlocker: options.ignoreBlocker,
    });
  }

  return (
    <SettingsListLayout
      open={search.open !== undefined}
      onClose={() => {
        go((prev) => ({ ...prev, open: undefined }));
      }}
      panel={
        search.open === undefined ? null : query.isPending ? (
          <Skeleton className="m-4 h-8 w-full" />
        ) : openMember ? (
          <MemberAccessPanel
            key={openMember.user.id}
            org={org}
            project={project}
            ref={ref}
            member={openMember}
            selfId={me.user.id}
            canEdit={canEdit}
            locked={locked}
            upgradeUrl={entitlements.upgradeUrl}
            onClose={() => {
              go((prev) => ({ ...prev, open: undefined }));
            }}
          />
        ) : (
          <SettingsPanel
            title="Member not found"
            onClose={() => {
              go((prev) => ({ ...prev, open: undefined }));
            }}
          >
            <p className="text-body text-muted-foreground">
              This member is not in the loaded list.
            </p>
          </SettingsPanel>
        )
      }
    >
      <SettingsPage
        title="Members"
        description="Org members and their access to this project. Org owners and admins always have admin access."
      >
        {query.isPending ? (
          <>
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
          </>
        ) : rows.length === 0 ? (
          <EmptyState icon={Users} title="No members yet" />
        ) : (
          <>
            <SettingsListToolbar
              label="project members"
              query={text}
              onQueryChange={(value) => {
                setText(value);
                go((prev) => ({ ...prev, q: value.trim() || undefined }), { replace: true });
              }}
              count={`${visible.length}${query.hasNextPage ? "+" : ""} members`}
            />
            {visible.length === 0 ? (
              <EmptyState icon={Users} title="No member matches" />
            ) : (
              <SettingsTableFrame rowCount={visible.length}>
                <DataTable
                  tableId="project-members"
                  ariaLabel="Project members"
                  columns={COLUMNS}
                  rows={visible}
                  getRowId={(member) => member.user.id}
                  selectedRowId={search.open}
                  onActivate={(member) => {
                    go((prev) => ({ ...prev, open: member.user.id }));
                  }}
                  onSelectionMove={(member) => {
                    go((prev) => ({ ...prev, open: member.user.id }), { replace: true });
                  }}
                  sorting={sorting}
                  onSortingChange={(next) => {
                    go(
                      (prev) => ({
                        ...prev,
                        sort: next?.id,
                        desc: next?.desc ? true : undefined,
                      }),
                      { replace: true },
                    );
                  }}
                  hasNextPage={query.hasNextPage}
                  isFetchingNextPage={query.isFetchingNextPage}
                  fetchNextPage={fetchNextPage}
                  emptyState={null}
                />
              </SettingsTableFrame>
            )}
          </>
        )}
      </SettingsPage>
    </SettingsListLayout>
  );
}
