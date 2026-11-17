import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { BellRing, Loader2, Plus } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { alertRulesQuery, channelsQuery } from "@/api/queries-alerts";
import type { Schema } from "@/api/types";
import { CANNOT_MANAGE_CHANNELS, ChannelForm } from "@/components/alerts/channel-form";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { Forbidden } from "@/components/forbidden";
import { FormError } from "@/components/form/form-error";
import { FormSuccess } from "@/components/form/form-success";
import { RelativeTime } from "@/components/relative-time";
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
import { type PillTone, StatusPill } from "@/components/settings/status-pill";
import { useProject } from "@/components/shell/project-context";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { channelKindLabel } from "@/lib/alerts";
import { canManageAlerts } from "@/lib/roles";
import { parseSettingsListSearch, type SettingsListSearch } from "@/lib/search/settings-list";

type Channel = Schema<"NotificationChannel">;

const SORTS = ["name", "last-delivery"] as const;

export const Route = createFileRoute("/$org/$project/settings/channels")({
  validateSearch: (search) => parseSettingsListSearch(search, SORTS),
  component: ChannelsPage,
});

const SORT_KEYS: Record<string, (channel: Channel) => SortValue> = {
  name: (channel) => channel.name,
  "last-delivery": (channel) => channel.last_delivery?.at,
};

/** The pill of a channel: a missing SMTP setup first, then the last delivery. */
function channelStatus(channel: Channel): { tone: PillTone; label: string; title?: string } {
  if (!channel.available) {
    return {
      tone: "warn",
      label: "SMTP not configured",
      title: "Email channels need SMTP (smtp_url). Ask an instance admin.",
    };
  }
  const last = channel.last_delivery;
  if (last?.status === "failed") {
    return { tone: "error", label: "Last delivery failed", title: last.error };
  }
  if (last?.status === "sent") {
    return { tone: "ok", label: "Delivered" };
  }
  return { tone: "unset", label: "No deliveries" };
}

const CHANNEL_COLUMNS: DataTableColumn<Channel>[] = [
  {
    id: "name",
    size: 200,
    header: "Name",
    accessorFn: (channel) => channel.name,
    enableSorting: true,
    cell: ({ row }) => <span className="truncate">{row.original.name}</span>,
  },
  {
    id: "kind",
    size: 120,
    header: "Kind",
    enableSorting: false,
    cell: ({ row }) => channelKindLabel(row.original.kind),
  },
  {
    id: "target",
    size: 220,
    header: "Target",
    enableSorting: false,
    meta: { grow: true },
    cell: ({ row }) => (
      <span className="block truncate font-mono text-caption">{row.original.target_hint}</span>
    ),
  },
  {
    id: "status",
    size: 200,
    header: "Status",
    enableSorting: false,
    cell: ({ row }) => {
      const status = channelStatus(row.original);
      return (
        <StatusPill tone={status.tone} title={status.title}>
          {status.label}
        </StatusPill>
      );
    },
  },
  {
    id: "last-delivery",
    size: 128,
    header: "Last delivery",
    accessorFn: (channel) => channel.last_delivery?.at,
    enableSorting: true,
    cell: ({ row }) =>
      row.original.last_delivery ? (
        <RelativeTime value={row.original.last_delivery.at} />
      ) : (
        <span className="text-muted-foreground">—</span>
      ),
  },
];

function ChannelsPage() {
  const { org, project, data } = useProject();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const canEdit = canManageAlerts(data.role);
  const query = useQuery(channelsQuery(org, project));

  if (query.error) {
    if (problemOf(query.error)?.status === 403) {
      return <Forbidden description="You don't have access to this project's channels." />;
    }
    throw query.error;
  }

  const channels = query.data?.data ?? [];
  const shown = sortRows(
    filterRows(channels, search.q, (channel) => [
      channel.name,
      channelKindLabel(channel.kind),
      channel.target_hint,
    ]),
    search.sort === undefined ? undefined : { id: search.sort, desc: search.desc === true },
    SORT_KEYS,
  );
  const sorting =
    search.sort === undefined ? undefined : { id: search.sort, desc: search.desc === true };
  const openId = search.open;
  const openChannel =
    openId === undefined || openId === "new"
      ? undefined
      : channels.find((channel) => channel.id === openId);

  function openPanel(id: string, replace = false) {
    void navigate({
      to: "/$org/$project/settings/channels",
      params: { org, project },
      search: (prev) => ({ ...prev, open: id }),
      replace,
    });
  }

  // Closing asks first when the form is dirty, unless the save or delete already went through.
  function closePanel(ignoreBlocker = false) {
    void navigate({
      to: "/$org/$project/settings/channels",
      params: { org, project },
      search: (prev) => ({ ...prev, open: undefined }),
      ignoreBlocker,
    });
  }

  return (
    <SettingsListLayout
      open={openId !== undefined}
      onClose={() => {
        closePanel();
      }}
      panel={
        openId === undefined ? null : openId === "new" ? (
          <SettingsPanel
            title="New channel"
            subtitle="Alert rules send their notifications to channels."
            onClose={() => {
              closePanel();
            }}
          >
            {canEdit ? (
              <ChannelForm
                org={org}
                project={project}
                canEdit={canEdit}
                onDone={() => {
                  closePanel(true);
                }}
              />
            ) : (
              <p className="text-body text-muted-foreground">{CANNOT_MANAGE_CHANNELS}</p>
            )}
          </SettingsPanel>
        ) : openChannel ? (
          <ChannelPanel
            key={openChannel.id}
            org={org}
            project={project}
            channel={openChannel}
            canEdit={canEdit}
            onClose={() => {
              closePanel();
            }}
            onDone={() => {
              closePanel(true);
            }}
          />
        ) : (
          <SettingsPanel
            title={query.isPending ? "Loading channel" : "Channel not found"}
            onClose={() => {
              closePanel();
            }}
          >
            {query.isPending ? (
              <Skeleton className="h-8 w-full" />
            ) : (
              <p className="text-body text-muted-foreground">
                This channel is not in the list any more.
              </p>
            )}
          </SettingsPanel>
        )
      }
    >
      <SettingsPage
        title="Notification channels"
        description="Where alert rules send their notifications."
        actions={
          <DisabledReason reason={canEdit ? undefined : CANNOT_MANAGE_CHANNELS}>
            <Button
              type="button"
              size="sm"
              disabled={!canEdit}
              onClick={() => {
                openPanel("new");
              }}
            >
              <Plus />
              New channel
            </Button>
          </DisabledReason>
        }
      >
        {query.isPending ? (
          <>
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
          </>
        ) : channels.length === 0 ? (
          <EmptyState
            icon={BellRing}
            title="No channels yet"
            description="Add Slack, a webhook or email to receive alert notifications."
          />
        ) : (
          <>
            <SettingsListToolbar
              label="channels"
              query={search.q ?? ""}
              onQueryChange={(next) => {
                void navigate({
                  to: "/$org/$project/settings/channels",
                  params: { org, project },
                  search: (prev) => ({ ...prev, q: next.trim() === "" ? undefined : next }),
                  replace: true,
                  ignoreBlocker: true,
                });
              }}
              count={
                search.q
                  ? `${shown.length} of ${channels.length} channels`
                  : `${channels.length} ${channels.length === 1 ? "channel" : "channels"}`
              }
            />
            <SettingsTableFrame rowCount={shown.length}>
              <DataTable
                tableId="channels"
                ariaLabel="Notification channels"
                columns={CHANNEL_COLUMNS}
                rows={shown}
                getRowId={(row) => row.id}
                selectedRowId={openId}
                onActivate={(row) => {
                  openPanel(row.id);
                }}
                onSelectionMove={(row) => {
                  openPanel(row.id, true);
                }}
                hasNextPage={false}
                isFetchingNextPage={false}
                fetchNextPage={() => undefined}
                sorting={sorting}
                onSortingChange={(next) => {
                  void navigate({
                    to: "/$org/$project/settings/channels",
                    params: { org, project },
                    search: (prev: SettingsListSearch) => ({
                      ...prev,
                      sort: next?.id,
                      desc: next?.desc ? true : undefined,
                    }),
                    replace: true,
                    ignoreBlocker: true,
                  });
                }}
                emptyState={<EmptyState icon={BellRing} title="No channel matches" />}
              />
            </SettingsTableFrame>
          </>
        )}
      </SettingsPage>
    </SettingsListLayout>
  );
}

/** The panel of an existing channel: its form, a test send, the last delivery and delete. */
function ChannelPanel({
  org,
  project,
  channel,
  canEdit,
  onClose,
  onDone,
}: {
  org: string;
  project: string;
  channel: Channel;
  canEdit: boolean;
  onClose: () => void;
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const [deleteOpen, setDeleteOpen] = useState(false);
  const remove = $api.useMutation("delete", "/api/v1/projects/{project}/channels/{id}");
  const test = $api.useMutation("post", "/api/v1/projects/{project}/channels/{id}/test");
  const reason = canEdit ? undefined : CANNOT_MANAGE_CHANNELS;
  const last = channel.last_delivery;

  return (
    <SettingsPanel
      title={channel.name}
      subtitle={channelKindLabel(channel.kind)}
      onClose={onClose}
      footer={
        <>
          <DisabledReason reason={reason}>
            <Button
              type="button"
              variant="outline"
              className="text-destructive"
              disabled={!canEdit || remove.isPending}
              onClick={() => {
                setDeleteOpen(true);
              }}
            >
              Delete
            </Button>
          </DisabledReason>
          <DisabledReason reason={reason}>
            <Button
              type="button"
              variant="outline"
              disabled={!canEdit || test.isPending}
              onClick={() => {
                test.mutate({
                  params: { path: { project: projectRef(org, project), id: channel.id } },
                });
              }}
            >
              {test.isPending ? <Loader2 className="animate-spin" aria-hidden="true" /> : null}
              Send test
            </Button>
          </DisabledReason>
        </>
      }
    >
      <ChannelForm
        org={org}
        project={project}
        channel={channel}
        canEdit={canEdit}
        onDone={onDone}
      />
      {last ? (
        <div className="flex flex-col gap-1 text-body text-muted-foreground">
          <span>
            Last delivery {last.status === "sent" ? "sent" : "failed"}{" "}
            <RelativeTime value={last.at} />
          </span>
          {last.error ? <span className="text-caption">{last.error}</span> : null}
        </div>
      ) : null}
      {test.isError ? (
        <FormError message={problemMessage(test.error, "Could not send the test.")} />
      ) : null}
      {test.data?.ok === true ? (
        <FormSuccess message="The channel accepted the test message." />
      ) : null}
      {test.data?.ok === false ? (
        <div className="flex flex-col gap-1">
          <FormError message="The channel did not accept the test message." />
          <p className="text-caption text-muted-foreground">{test.data.error}</p>
        </div>
      ) : null}
      <TypeToConfirmDialog
        open={deleteOpen}
        onOpenChange={(next) => {
          setDeleteOpen(next);
          if (!next) {
            remove.reset();
          }
        }}
        title="Delete channel?"
        description={
          <>
            <span className="font-medium">{channel.name}</span> will be removed from every alert
            rule that uses it.
          </>
        }
        expected={channel.name}
        inputLabel={`Type "${channel.name}" to confirm`}
        confirmLabel="Delete"
        pending={remove.isPending}
        error={
          remove.error ? problemMessage(remove.error, "Could not delete the channel.") : undefined
        }
        onConfirm={() => {
          remove.mutate(
            { params: { path: { project: projectRef(org, project), id: channel.id } } },
            {
              onSuccess: () => {
                setDeleteOpen(false);
                remove.reset();
                void queryClient.invalidateQueries({
                  queryKey: channelsQuery(org, project).queryKey,
                });
                void queryClient.invalidateQueries({
                  queryKey: alertRulesQuery(org, project).queryKey,
                });
                onDone();
              },
            },
          );
        }}
      />
    </SettingsPanel>
  );
}
