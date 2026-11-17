import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { Loader2 } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { meQuery } from "@/api/queries";
import { orgMembersInit, orgMembersKey } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { DangerZone } from "@/components/deletions/danger-zone";
import { DisabledReason } from "@/components/disabled-reason";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { RelativeTime } from "@/components/relative-time";
import { SettingsFormCard } from "@/components/settings/settings-card";
import {
  filterRows,
  SettingsListToolbar,
  SettingsPanel,
  SettingsTableFrame,
  type SortValue,
  sortRows,
} from "@/components/settings/settings-list";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import {
  atLeast,
  canChangeRole,
  canRemoveMember,
  grantableRoles,
  ORG_ROLES,
  type OrgRole,
  roleLabel,
  roleRank,
} from "@/lib/roles";
import type { SettingsListSearch } from "@/lib/search/settings-list";

type Member = Schema<"OrgMember">;

/** The members of the organization, read a page at a time. The list and the panel share it. */
export function useOrgMembers(org: string) {
  return $api.useInfiniteQuery("get", "/api/v1/orgs/{org}/members", orgMembersInit(org), {
    pageParamName: "cursor",
    initialPageParam: null,
    getNextPageParam: (page) => page.next_cursor ?? undefined,
  });
}

const SORT_KEYS: Record<string, (member: Member) => SortValue> = {
  name: (member) => member.user.name ?? member.user.email,
  role: (member) => roleRank(member.role),
  // RFC 3339 times with a fixed-width fraction sort as text.
  joined: (member) => member.joined_at,
};

function memberColumns(me: string): DataTableColumn<Member>[] {
  return [
    {
      id: "name",
      size: 200,
      header: "Name",
      accessorFn: (member) => member.user.name ?? member.user.email,
      enableSorting: true,
      cell: ({ row }) => (
        <div className="flex min-w-0 items-center gap-2">
          <span className="truncate">{row.original.user.name ?? "—"}</span>
          {row.original.user.id === me ? <Badge variant="secondary">You</Badge> : null}
        </div>
      ),
    },
    {
      id: "email",
      size: 240,
      header: "Email",
      enableSorting: false,
      meta: { grow: true },
      cell: ({ row }) => (
        <span className="truncate text-muted-foreground">{row.original.user.email}</span>
      ),
    },
    {
      id: "role",
      size: 120,
      header: "Role",
      accessorFn: (member) => roleRank(member.role),
      enableSorting: true,
      cell: ({ row }) => roleLabel(row.original.role),
    },
    {
      id: "joined",
      size: 120,
      header: "Joined",
      accessorFn: (member) => member.joined_at,
      enableSorting: true,
      cell: ({ row }) => <RelativeTime value={row.original.joined_at} />,
    },
  ];
}

const noop = () => undefined;

export function OrgMembers({
  org,
  me,
  search,
  onSearchChange,
  onOpen,
}: {
  org: string;
  me: string;
  search: SettingsListSearch;
  onSearchChange: (change: Partial<SettingsListSearch>) => void;
  onOpen: (id: string, replace?: boolean) => void;
}) {
  const query = useOrgMembers(org);
  // Typing keeps its own text: the URL holds the trimmed query, which would drop a trailing space.
  const [text, setText] = useState(search.q ?? "");
  useEffect(() => {
    setText((current) => (current.trim() === (search.q ?? "") ? current : (search.q ?? "")));
  }, [search.q]);
  const columns = useMemo(() => memberColumns(me), [me]);

  if (query.error) {
    throw query.error;
  }

  const rows = query.data?.pages.flatMap((page) => page.data) ?? [];
  const sorting =
    search.sort === undefined ? undefined : { id: search.sort, desc: search.desc === true };
  const visible = sortRows(
    filterRows(rows, search.q, (member) => [member.user.name, member.user.email]),
    sorting,
    SORT_KEYS,
  );

  return (
    <div className="flex flex-col gap-3">
      <SettingsListToolbar
        label="members"
        query={text}
        onQueryChange={(next) => {
          setText(next);
          onSearchChange({ q: next.trim() || undefined });
        }}
        count={
          query.isPending ? undefined : `${rows.length} ${rows.length === 1 ? "member" : "members"}`
        }
      />
      {query.isPending ? (
        <>
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </>
      ) : visible.length === 0 ? (
        <p className="text-body text-muted-foreground">No member matches.</p>
      ) : (
        <SettingsTableFrame rowCount={visible.length}>
          <DataTable
            tableId="org-members"
            ariaLabel="Members"
            columns={columns}
            rows={visible}
            getRowId={(member) => member.user.id}
            selectedRowId={search.open}
            onActivate={(member) => {
              onOpen(member.user.id);
            }}
            onSelectionMove={(member) => {
              onOpen(member.user.id, true);
            }}
            hasNextPage={false}
            isFetchingNextPage={false}
            fetchNextPage={noop}
            sorting={sorting}
            onSortingChange={(next) => {
              onSearchChange({ sort: next?.id, desc: next?.desc === true ? true : undefined });
            }}
            emptyState={null}
          />
        </SettingsTableFrame>
      )}
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
    </div>
  );
}

/** The panel of one member: the role form, and removing the member or leaving. */
export function MemberPanel({
  org,
  userId,
  actorRole,
  me,
  onClose,
}: {
  org: string;
  userId: string;
  actorRole: OrgRole | undefined;
  me: string;
  onClose: (ignoreBlocker?: boolean) => void;
}) {
  const query = useOrgMembers(org);
  const { hasNextPage, isFetchingNextPage, fetchNextPage } = query;
  const member = query.data?.pages
    .flatMap((page) => page.data)
    .find((item) => item.user.id === userId);
  // The member may sit on a page that is not loaded yet.
  useEffect(() => {
    if (member === undefined && hasNextPage && !isFetchingNextPage) {
      void fetchNextPage();
    }
  }, [member, hasNextPage, isFetchingNextPage, fetchNextPage]);

  if (member === undefined) {
    return (
      <SettingsPanel title="Member" onClose={() => onClose()}>
        {query.isPending || hasNextPage ? (
          <Skeleton className="h-8 w-full" />
        ) : (
          <p className="text-body text-muted-foreground">
            This person is no longer a member of the organization.
          </p>
        )}
      </SettingsPanel>
    );
  }

  return (
    <SettingsPanel
      title={member.user.name ?? member.user.email}
      subtitle={member.user.name ? member.user.email : undefined}
      onClose={() => onClose()}
    >
      <RoleForm key={member.user.id} org={org} member={member} actorRole={actorRole} />
      <RemoveMember
        key={member.user.id}
        org={org}
        member={member}
        actorRole={actorRole}
        me={me}
        onClose={onClose}
      />
    </SettingsPanel>
  );
}

function roleErrorMessage(error: unknown): string {
  const problem = problemOf(error);
  if (problem?.code === "plan_limit") {
    return problemMessage(error, "Your plan does not allow another seat.");
  }
  if (problem?.status === 409) {
    return "An organization needs at least one owner. Make someone else an owner first.";
  }
  if (problem?.status === 403) {
    return "You can't give that role.";
  }
  return problemMessage(error, "Could not change the role.");
}

/** Changing a member's role also changes their project access, so those cached entries go. */
function useChangeRole(org: string) {
  const queryClient = useQueryClient();
  return $api.useMutation("patch", "/api/v1/orgs/{org}/members/{user}", {
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: orgMembersKey(org) });
      void queryClient.invalidateQueries({ queryKey: meQuery.queryKey });
      // The new org role changes project access: drop every cached project and
      // project-member entry ($api keys are [method, path, init], so these match
      // by prefix whatever the path parameters are).
      void queryClient.invalidateQueries({ queryKey: ["get", "/api/v1/projects/{project}"] });
      void queryClient.invalidateQueries({
        queryKey: ["get", "/api/v1/projects/{project}/members"],
      });
    },
  });
}

function RoleForm({
  org,
  member,
  actorRole,
}: {
  org: string;
  member: Member;
  actorRole: OrgRole | undefined;
}) {
  const change = useChangeRole(org);
  const upgradeUrl = useOrgEntitlements(org).upgradeUrl;
  const [role, setRole] = useState<OrgRole>(member.role);
  const roleItems = grantableRoles(actorRole);
  const items = roleItems.includes(member.role) ? roleItems : [member.role, ...roleItems];
  const roleReason = !atLeast(actorRole, "admin")
    ? "Only admins and owners can change roles"
    : member.role === "owner" && actorRole !== "owner"
      ? "Only an owner can change an owner's role"
      : undefined;
  const planLimited = problemOf(change.error)?.code === "plan_limit";

  return (
    <SettingsFormCard
      title="Role"
      ariaLabel={`Role of ${member.user.email}`}
      description="Applies to every project of this organization, unless a project overrides it."
      dirty={role !== member.role}
      pending={change.isPending}
      saved={change.isSuccess}
      error={change.error ? roleErrorMessage(change.error) : undefined}
      disabledReason={roleReason}
      footerExtra={planLimited ? <UpgradeButton url={upgradeUrl} /> : undefined}
      onSubmit={() => {
        change.mutate({ params: { path: { org, user: member.user.id } }, body: { role } });
      }}
    >
      <div className="flex flex-col gap-2">
        <Label htmlFor="member-role">Role</Label>
        <DisabledReason reason={roleReason}>
          <Select
            value={role}
            onValueChange={(value) => {
              const next = ORG_ROLES.find((item) => item === value);
              if (next) {
                setRole(next);
              }
            }}
            disabled={!canChangeRole(actorRole, member.role) || change.isPending}
          >
            <SelectTrigger id="member-role" className="w-fit">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {items.map((item) => (
                <SelectItem key={item} value={item}>
                  {roleLabel(item)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </DisabledReason>
      </div>
    </SettingsFormCard>
  );
}

function removeErrorMessage(error: unknown): string {
  return problemOf(error)?.status === 409
    ? "An organization needs at least one owner. Make someone else an owner first."
    : problemMessage(error, "Could not remove the member.");
}

function RemoveMember({
  org,
  member,
  actorRole,
  me,
  onClose,
}: {
  org: string;
  member: Member;
  actorRole: OrgRole | undefined;
  me: string;
  onClose: (ignoreBlocker?: boolean) => void;
}) {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const [open, setOpen] = useState(false);
  const remove = $api.useMutation("delete", "/api/v1/orgs/{org}/members/{user}");
  const self = member.user.id === me;
  const { email } = member.user;
  const name = member.user.name ?? email;
  const canRemove = canRemoveMember(actorRole, member.role, self);
  const removeReason = canRemove
    ? undefined
    : !atLeast(actorRole, "admin")
      ? "Only admins and owners can remove members"
      : "Only an owner can remove an owner";
  const description = self
    ? "You will lose access to every project in this organization."
    : `${name} loses access to every project in this organization.`;

  return (
    <DangerZone
      title={self ? "Leave organization" : "Remove from organization"}
      description={self ? "You lose access to every project." : `${name} loses access.`}
      buttonLabel={self ? "Leave…" : "Remove…"}
      disabledReason={removeReason}
      onOpen={() => {
        remove.reset();
        setOpen(true);
      }}
    >
      <TypeToConfirmDialog
        open={open}
        onOpenChange={(next) => {
          setOpen(next);
          if (!next) {
            remove.reset();
          }
        }}
        title={self ? "Leave organization?" : "Remove member?"}
        description={description}
        expected={email}
        inputLabel={`Type "${email}" to confirm`}
        confirmLabel={self ? "Leave" : "Remove"}
        pending={remove.isPending}
        error={remove.error ? removeErrorMessage(remove.error) : undefined}
        onConfirm={() => {
          remove.mutate(
            { params: { path: { org, user: member.user.id } } },
            {
              onSuccess: async () => {
                setOpen(false);
                remove.reset();
                if (self) {
                  await queryClient.refetchQueries({ queryKey: meQuery.queryKey, type: "all" });
                  void navigate({ to: "/", ignoreBlocker: true });
                  return;
                }
                void queryClient.invalidateQueries({ queryKey: orgMembersKey(org) });
                onClose(true);
              },
            },
          );
        }}
      />
    </DangerZone>
  );
}
