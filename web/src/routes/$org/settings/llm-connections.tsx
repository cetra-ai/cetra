import { useQueryClient } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { PlugZap } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { orgAssistantKey } from "@/api/queries-assistant";
import { connectionsInit, connectionsKey } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { DangerZone } from "@/components/deletions/danger-zone";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { RelativeTime } from "@/components/relative-time";
import { ConnectionForm, structuredOutputLabel } from "@/components/settings/connection-form";
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
import { StatusPill } from "@/components/settings/status-pill";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { providerLabel } from "@/lib/llm-adapters";
import { canManageConnections } from "@/lib/roles";
import { parseSettingsListSearch, type SettingsListSearch } from "@/lib/search/settings-list";

type Connection = Schema<"LlmConnection">;

const SORTS = ["name", "provider", "model", "status", "updated"] as const;

export const Route = createFileRoute("/$org/settings/llm-connections")({
  validateSearch: (search) => parseSettingsListSearch(search, SORTS),
  component: LlmConnectionsPage,
});

function deleteWarning(name: string): string {
  return `Evaluators that use ${name} stop working. If the assistant uses it, the assistant turns off.`;
}

const COLUMNS: DataTableColumn<Connection>[] = [
  {
    id: "name",
    size: 200,
    header: "Name",
    // The accessor makes the column sortable; the rows are sorted by SORT_KEYS.
    accessorFn: (connection) => connection.name,
    enableSorting: true,
    meta: { grow: true },
    cell: ({ row }) => <span className="truncate font-medium">{row.original.name}</span>,
  },
  {
    id: "provider",
    size: 180,
    header: "Provider",
    accessorFn: (connection) => providerLabel(connection),
    enableSorting: true,
    cell: ({ row }) => providerLabel(row.original),
  },
  {
    id: "model",
    size: 180,
    header: "Default model",
    accessorFn: (connection) => connection.default_model,
    enableSorting: true,
    cell: ({ row }) =>
      row.original.default_model ?? <span className="text-muted-foreground">—</span>,
  },
  {
    id: "status",
    size: 120,
    header: "Status",
    accessorFn: (connection) => connection.status,
    enableSorting: true,
    cell: ({ row }) =>
      row.original.status === "ok" ? (
        <StatusPill tone="ok">OK</StatusPill>
      ) : (
        <StatusPill tone="error" title={row.original.last_error}>
          Failing
        </StatusPill>
      ),
  },
  {
    id: "last-error",
    size: 120,
    header: "Last error at",
    enableSorting: false,
    cell: ({ row }) =>
      row.original.last_error_at ? (
        <RelativeTime value={row.original.last_error_at} />
      ) : (
        <span className="text-muted-foreground">—</span>
      ),
  },
  {
    id: "updated",
    size: 120,
    header: "Updated",
    accessorFn: (connection) => connection.updated_at,
    enableSorting: true,
    cell: ({ row }) => <RelativeTime value={row.original.updated_at} />,
  },
];

const SORT_KEYS: Record<string, (connection: Connection) => SortValue> = {
  name: (connection) => connection.name,
  provider: (connection) => providerLabel(connection),
  model: (connection) => connection.default_model,
  status: (connection) => connection.status,
  // RFC 3339 times with a fixed-width fraction sort as text.
  updated: (connection) => connection.updated_at,
};

const noop = () => undefined;

function LlmConnectionsPage() {
  const { org } = Route.useParams();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  const role = me.orgs.find((item) => item.slug === org)?.role;
  const canManage = canManageConnections(role);
  const query = $api.useQuery("get", "/api/v1/orgs/{org}/llm-connections", connectionsInit(org));

  if (query.error) {
    throw query.error;
  }

  const rows = query.data?.data ?? [];
  const openId = search.open;
  const selected =
    openId === undefined || openId === "new"
      ? undefined
      : rows.find((connection) => connection.id === openId);

  function update(change: Partial<SettingsListSearch>) {
    void navigate({ search: (prev) => ({ ...prev, ...change }), replace: true });
  }

  function openRow(id: string, replace = false) {
    void navigate({ search: (prev) => ({ ...prev, open: id }), replace });
  }

  function closePanel(ignoreBlocker = false) {
    void navigate({ search: (prev) => ({ ...prev, open: undefined }), ignoreBlocker });
  }

  let panel: ReactNode = null;
  if (openId === "new") {
    panel = (
      <SettingsPanel title="New connection" onClose={() => closePanel()}>
        {canManage ? (
          <ConnectionForm org={org} connection={undefined} onCreated={() => closePanel(true)} />
        ) : (
          <p className="text-body text-muted-foreground">
            Only admins and owners can add connections.
          </p>
        )}
      </SettingsPanel>
    );
  } else if (selected !== undefined) {
    panel = (
      <ConnectionDetail
        key={selected.id}
        org={org}
        connection={selected}
        canManage={canManage}
        onClose={closePanel}
      />
    );
  } else if (openId !== undefined) {
    panel = (
      <SettingsPanel title="Connection" onClose={() => closePanel()}>
        {query.isFetching ? (
          <Skeleton className="h-8 w-full" />
        ) : (
          <p className="text-body text-muted-foreground">This connection no longer exists.</p>
        )}
      </SettingsPanel>
    );
  }

  return (
    <SettingsListLayout open={openId !== undefined} onClose={() => closePanel()} panel={panel}>
      <SettingsPage
        width="wide"
        title="LLM connections"
        description="Provider keys that evaluators use. Keys are stored encrypted and never shown again."
        actions={
          <DisabledReason
            reason={canManage ? undefined : "Only admins and owners can add connections"}
          >
            <Button type="button" disabled={!canManage} onClick={() => openRow("new")}>
              Add connection
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
        ) : rows.length === 0 ? (
          <EmptyState
            icon={PlugZap}
            title="No connections yet"
            description="Add one to run LLM judges."
          />
        ) : (
          <ConnectionsList rows={rows} search={search} onSearchChange={update} onOpen={openRow} />
        )}
      </SettingsPage>
    </SettingsListLayout>
  );
}

function ConnectionsList({
  rows,
  search,
  onSearchChange,
  onOpen,
}: {
  rows: Connection[];
  search: SettingsListSearch;
  onSearchChange: (change: Partial<SettingsListSearch>) => void;
  onOpen: (id: string, replace?: boolean) => void;
}) {
  // Typing keeps its own text: the URL holds the trimmed query, which would drop a trailing space.
  const [text, setText] = useState(search.q ?? "");
  useEffect(() => {
    setText((current) => (current.trim() === (search.q ?? "") ? current : (search.q ?? "")));
  }, [search.q]);

  const sorting =
    search.sort === undefined ? undefined : { id: search.sort, desc: search.desc === true };
  const visible = sortRows(
    filterRows(rows, search.q, (connection) => [
      connection.name,
      providerLabel(connection),
      connection.default_model,
    ]),
    sorting,
    SORT_KEYS,
  );

  return (
    <div className="flex flex-col gap-3">
      <SettingsListToolbar
        label="connections"
        query={text}
        onQueryChange={(next) => {
          setText(next);
          onSearchChange({ q: next.trim() || undefined });
        }}
        count={`${rows.length} ${rows.length === 1 ? "connection" : "connections"}`}
      />
      {visible.length === 0 ? (
        <p className="text-body text-muted-foreground">No connection matches.</p>
      ) : (
        <SettingsTableFrame rowCount={visible.length}>
          <DataTable
            tableId="llm-connections"
            ariaLabel="LLM connections"
            columns={COLUMNS}
            rows={visible}
            getRowId={(connection) => connection.id}
            selectedRowId={search.open}
            onActivate={(connection) => {
              onOpen(connection.id);
            }}
            onSelectionMove={(connection) => {
              onOpen(connection.id, true);
            }}
            hasNextPage={false}
            isFetchingNextPage={false}
            fetchNextPage={noop}
            sorting={sorting}
            onSortingChange={(next) => {
              onSearchChange({
                sort: next?.id,
                desc: next?.desc === true ? true : undefined,
              });
            }}
            emptyState={null}
          />
        </SettingsTableFrame>
      )}
    </div>
  );
}

/** The panel of one connection: its edit form and delete for admins, its details otherwise. */
function ConnectionDetail({
  org,
  connection,
  canManage,
  onClose,
}: {
  org: string;
  connection: Connection;
  canManage: boolean;
  onClose: (ignoreBlocker?: boolean) => void;
}) {
  const queryClient = useQueryClient();
  const [deleteOpen, setDeleteOpen] = useState(false);
  const remove = $api.useMutation("delete", "/api/v1/orgs/{org}/llm-connections/{id}", {
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: connectionsKey(org) });
      // Deleting the connection turns the assistant off when it uses it (spec 09 § Assistant).
      void queryClient.invalidateQueries({ queryKey: orgAssistantKey(org) });
      void queryClient.invalidateQueries({
        queryKey: ["get", "/api/v1/projects/{project}/assistant"],
      });
    },
  });
  const failing = connection.status === "failing";

  return (
    <SettingsPanel
      title={connection.name}
      subtitle={
        <span className="flex flex-wrap items-center gap-2">
          {providerLabel(connection)}
          {failing ? (
            <StatusPill tone="error" title={connection.last_error}>
              Failing
            </StatusPill>
          ) : (
            <StatusPill tone="ok">OK</StatusPill>
          )}
          {connection.last_tested_at === undefined ? null : connection.last_test_ok === false ? (
            <span>
              Last test failed <RelativeTime value={connection.last_tested_at} />
            </span>
          ) : (
            <span>
              Tested <RelativeTime value={connection.last_tested_at} />
            </span>
          )}
        </span>
      }
      onClose={() => onClose()}
    >
      {canManage ? (
        <>
          <ConnectionForm org={org} connection={connection} onCreated={noop} />
          <DangerZone
            title="Delete connection"
            description={deleteWarning(connection.name)}
            buttonLabel="Delete…"
            onOpen={() => {
              remove.reset();
              setDeleteOpen(true);
            }}
          >
            <TypeToConfirmDialog
              open={deleteOpen}
              onOpenChange={(next) => {
                setDeleteOpen(next);
                if (!next) {
                  remove.reset();
                }
              }}
              title="Delete connection?"
              description={deleteWarning(connection.name)}
              expected={connection.name}
              inputLabel={`Type "${connection.name}" to confirm`}
              confirmLabel="Delete"
              pending={remove.isPending}
              error={
                remove.error
                  ? problemMessage(remove.error, "Could not delete the connection.")
                  : undefined
              }
              onConfirm={() => {
                remove.mutate(
                  { params: { path: { org, id: connection.id } } },
                  {
                    onSuccess: () => {
                      setDeleteOpen(false);
                      onClose(true);
                    },
                  },
                );
              }}
            />
          </DangerZone>
        </>
      ) : (
        <ReadOnlyDetails connection={connection} />
      )}
    </SettingsPanel>
  );
}

function ReadOnlyDetails({ connection }: { connection: Connection }) {
  const details: [string, string][] = [
    ["Provider", providerLabel(connection)],
    ["Base URL", connection.base_url ?? "Default"],
    ["Default model", connection.default_model ?? "—"],
    ["API key", connection.api_key_hint ? `••••${connection.api_key_hint}` : "••••"],
    ["Structured output", structuredOutputLabel(connection.structured_output)],
    ["Max concurrency", String(connection.max_concurrency)],
    ["Requests per minute", String(connection.requests_per_minute)],
    [
      "Extra headers",
      connection.extra_header_names.length > 0 ? connection.extra_header_names.join(", ") : "None",
    ],
  ];
  return (
    <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-6 gap-y-3 text-body">
      {details.map(([label, value]) => (
        <div key={label} className="contents">
          <dt className="text-muted-foreground">{label}</dt>
          <dd className="break-all">{value}</dd>
        </div>
      ))}
    </dl>
  );
}
