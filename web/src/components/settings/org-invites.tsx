import { useQueryClient } from "@tanstack/react-query";
import { Loader2 } from "lucide-react";
import { useEffect, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { orgInvitesInit, orgInvitesKey } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { DangerZone } from "@/components/deletions/danger-zone";
import { RelativeTime } from "@/components/relative-time";
import {
  filterRows,
  SettingsListToolbar,
  SettingsPanel,
  SettingsTableFrame,
} from "@/components/settings/settings-list";
import { TimeText } from "@/components/time-text";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { roleLabel } from "@/lib/roles";
import type { SettingsListSearch } from "@/lib/search/settings-list";

type Invite = Schema<"Invite">;

/** The pending invitations of the organization, read a page at a time. The list and the panel share it. */
export function useOrgInvites(org: string) {
  return $api.useInfiniteQuery("get", "/api/v1/orgs/{org}/invites", orgInvitesInit(org), {
    pageParamName: "cursor",
    initialPageParam: null,
    getNextPageParam: (page) => page.next_cursor ?? undefined,
  });
}

const COLUMNS: DataTableColumn<Invite>[] = [
  {
    id: "email",
    size: 240,
    header: "Email",
    enableSorting: false,
    meta: { grow: true },
    cell: ({ row }) => <span className="truncate">{row.original.email}</span>,
  },
  {
    id: "role",
    size: 120,
    header: "Role",
    enableSorting: false,
    cell: ({ row }) => roleLabel(row.original.role),
  },
  {
    id: "invited-by",
    size: 180,
    header: "Invited by",
    enableSorting: false,
    cell: ({ row }) =>
      row.original.invited_by?.name ??
      row.original.invited_by?.email ?? <span className="text-muted-foreground">—</span>,
  },
  {
    id: "created",
    size: 120,
    header: "Invited",
    enableSorting: false,
    cell: ({ row }) => <RelativeTime value={row.original.created_at} />,
  },
  {
    id: "expires",
    size: 160,
    header: "Expires",
    enableSorting: false,
    cell: ({ row }) => <TimeText value={row.original.expires_at} style="datetime" />,
  },
];

const noop = () => undefined;

export function OrgInvites({
  org,
  search,
  onSearchChange,
  onOpen,
}: {
  org: string;
  search: SettingsListSearch;
  onSearchChange: (change: Partial<SettingsListSearch>) => void;
  onOpen: (id: string, replace?: boolean) => void;
}) {
  const query = useOrgInvites(org);
  // Typing keeps its own text: the URL holds the trimmed query, which would drop a trailing space.
  const [text, setText] = useState(search.q ?? "");
  useEffect(() => {
    setText((current) => (current.trim() === (search.q ?? "") ? current : (search.q ?? "")));
  }, [search.q]);

  if (query.error) {
    throw query.error;
  }

  const rows = query.data?.pages.flatMap((page) => page.data) ?? [];
  const visible = filterRows(rows, search.q, (invite) => [
    invite.email,
    invite.invited_by?.name,
    invite.invited_by?.email,
  ]);

  return (
    <div className="flex flex-col gap-3">
      <SettingsListToolbar
        label="invitations"
        query={text}
        onQueryChange={(next) => {
          setText(next);
          onSearchChange({ q: next.trim() || undefined });
        }}
        count={
          query.isPending
            ? undefined
            : `${rows.length} pending ${rows.length === 1 ? "invitation" : "invitations"}`
        }
      />
      {query.isPending ? (
        <>
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </>
      ) : rows.length === 0 ? (
        <p className="text-body text-muted-foreground">No pending invitations.</p>
      ) : visible.length === 0 ? (
        <p className="text-body text-muted-foreground">No invitation matches.</p>
      ) : (
        <SettingsTableFrame rowCount={visible.length}>
          <DataTable
            tableId="org-invites"
            ariaLabel="Pending invitations"
            columns={COLUMNS}
            rows={visible}
            getRowId={(invite) => invite.id}
            selectedRowId={search.open}
            onActivate={(invite) => {
              onOpen(invite.id);
            }}
            onSelectionMove={(invite) => {
              onOpen(invite.id, true);
            }}
            hasNextPage={false}
            isFetchingNextPage={false}
            fetchNextPage={noop}
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

/** The panel of one pending invitation: its details, and revoking it. */
export function InvitePanel({
  org,
  inviteId,
  onClose,
}: {
  org: string;
  inviteId: string;
  onClose: (ignoreBlocker?: boolean) => void;
}) {
  const query = useOrgInvites(org);
  const { hasNextPage, isFetchingNextPage, fetchNextPage } = query;
  const invite = query.data?.pages
    .flatMap((page) => page.data)
    .find((item) => item.id === inviteId);
  // The invitation may sit on a page that is not loaded yet.
  useEffect(() => {
    if (invite === undefined && hasNextPage && !isFetchingNextPage) {
      void fetchNextPage();
    }
  }, [invite, hasNextPage, isFetchingNextPage, fetchNextPage]);

  if (invite === undefined) {
    return (
      <SettingsPanel title="Invitation" onClose={() => onClose()}>
        {query.isPending || hasNextPage ? (
          <Skeleton className="h-8 w-full" />
        ) : (
          <p className="text-body text-muted-foreground">This invitation no longer exists.</p>
        )}
      </SettingsPanel>
    );
  }

  return (
    <SettingsPanel
      title={invite.email}
      subtitle={`Invited as ${roleLabel(invite.role)}`}
      onClose={() => onClose()}
    >
      <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-6 gap-y-3 text-body">
        <dt className="text-muted-foreground">Invited by</dt>
        <dd>{invite.invited_by?.name ?? invite.invited_by?.email ?? "—"}</dd>
        <dt className="text-muted-foreground">Invited</dt>
        <dd>
          <RelativeTime value={invite.created_at} />
        </dd>
        <dt className="text-muted-foreground">Expires</dt>
        <dd>
          <TimeText value={invite.expires_at} style="datetime" />
        </dd>
      </dl>
      <RevokeInvite key={invite.id} org={org} invite={invite} onClose={onClose} />
    </SettingsPanel>
  );
}

function RevokeInvite({
  org,
  invite,
  onClose,
}: {
  org: string;
  invite: Invite;
  onClose: (ignoreBlocker?: boolean) => void;
}) {
  const queryClient = useQueryClient();
  const [open, setOpen] = useState(false);
  const revoke = $api.useMutation("delete", "/api/v1/orgs/{org}/invites/{id}", {
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: orgInvitesKey(org) });
    },
  });

  return (
    <DangerZone
      title="Revoke invitation"
      description="The link stops working."
      buttonLabel="Revoke…"
      onOpen={() => {
        revoke.reset();
        setOpen(true);
      }}
    >
      <TypeToConfirmDialog
        open={open}
        onOpenChange={(next) => {
          setOpen(next);
          if (!next) {
            revoke.reset();
          }
        }}
        title="Revoke invitation?"
        description={<>The link sent to {invite.email} stops working.</>}
        expected={invite.email}
        inputLabel={`Type "${invite.email}" to confirm`}
        confirmLabel="Revoke"
        pending={revoke.isPending}
        error={
          revoke.error
            ? problemMessage(revoke.error, "Could not revoke the invitation.")
            : undefined
        }
        onConfirm={() => {
          revoke.mutate(
            { params: { path: { org, id: invite.id } } },
            {
              onSuccess: () => {
                setOpen(false);
                revoke.reset();
                onClose(true);
              },
            },
          );
        }}
      />
    </DangerZone>
  );
}
