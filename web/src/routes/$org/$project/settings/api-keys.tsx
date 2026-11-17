import { useQueryClient } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { KeyRound, Plus } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { apiKeysInit, apiKeysKey } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { CreatedBy } from "@/components/created-by";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { EmptyState } from "@/components/empty-state";
import { Forbidden } from "@/components/forbidden";
import { RelativeTime } from "@/components/relative-time";
import { CreateKeyDialog } from "@/components/settings/create-key-dialog";
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
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { atLeast } from "@/lib/roles";
import { parseSettingsListSearch } from "@/lib/search/settings-list";

type ApiKey = Schema<"ApiKey">;

const EXPIRY_SOON_MS = 7 * 86_400_000;

export const Route = createFileRoute("/$org/$project/settings/api-keys")({
  validateSearch: (search) =>
    parseSettingsListSearch(search, ["name", "last_used", "created", "expires"]),
  component: ApiKeysPage,
});

function ApiKeysPage() {
  const { org, project, data } = useProject();
  const [creating, setCreating] = useState(false);

  if (!atLeast(data.role, "admin")) {
    return <Forbidden description="Only project admins can manage API keys." />;
  }

  return (
    <>
      <ApiKeysList org={org} project={project} onCreate={() => setCreating(true)} />
      <CreateKeyDialog org={org} project={project} open={creating} onOpenChange={setCreating} />
    </>
  );
}

/** Milliseconds of an RFC 3339 time, or undefined when it is absent or unreadable. */
function timeOf(value: string | undefined): number | undefined {
  if (value === undefined) {
    return undefined;
  }
  const time = Date.parse(value);
  return Number.isNaN(time) ? undefined : time;
}

/** Active, expiring within a week, or expired. A key without an expiry is active. */
function keyStatus(key: ApiKey, now: number): { tone: PillTone; label: string } {
  const expires = timeOf(key.expires_at);
  if (expires !== undefined && expires < now) {
    return { tone: "error", label: "Expired" };
  }
  if (expires !== undefined && expires - now < EXPIRY_SOON_MS) {
    return { tone: "warn", label: "Expires soon" };
  }
  return { tone: "ok", label: "Active" };
}

function StatusCell({ keyRow }: { keyRow: ApiKey }) {
  const status = keyStatus(keyRow, Date.now());
  return <StatusPill tone={status.tone}>{status.label}</StatusPill>;
}

const COLUMNS: DataTableColumn<ApiKey>[] = [
  {
    id: "name",
    size: 200,
    header: "Name",
    enableSorting: true,
    cell: ({ row }) => row.original.name ?? "—",
  },
  {
    id: "key",
    size: 180,
    header: "Key",
    enableSorting: false,
    cell: ({ row }) => <span className="font-mono">{row.original.prefix}…</span>,
  },
  {
    id: "kind",
    size: 96,
    header: "Kind",
    enableSorting: false,
    cell: ({ row }) => (
      <Badge variant="secondary">{row.original.kind === "secret" ? "Secret" : "Public"}</Badge>
    ),
  },
  {
    id: "scopes",
    size: 200,
    header: "Scopes",
    enableSorting: false,
    cell: ({ row }) => row.original.scopes.join(", "),
  },
  {
    id: "status",
    size: 120,
    header: "Status",
    enableSorting: false,
    cell: ({ row }) => <StatusCell keyRow={row.original} />,
  },
  {
    id: "last_used",
    size: 120,
    header: "Last used",
    enableSorting: true,
    cell: ({ row }) =>
      row.original.last_used_at ? <RelativeTime value={row.original.last_used_at} /> : "Never",
  },
  {
    id: "created",
    size: 120,
    header: "Created",
    enableSorting: true,
    cell: ({ row }) => <RelativeTime value={row.original.created_at} />,
  },
  {
    id: "expires",
    size: 120,
    header: "Expires",
    enableSorting: true,
    cell: ({ row }) =>
      row.original.expires_at ? <RelativeTime value={row.original.expires_at} /> : "Never",
  },
];

const SORT_KEYS: Record<string, (key: ApiKey) => SortValue> = {
  name: (key) => key.name?.toLowerCase(),
  last_used: (key) => timeOf(key.last_used_at),
  created: (key) => timeOf(key.created_at),
  expires: (key) => timeOf(key.expires_at),
};

function ApiKeysList({
  org,
  project,
  onCreate,
}: {
  org: string;
  project: string;
  onCreate: () => void;
}) {
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const [text, setText] = useState(search.q ?? "");
  // The search text follows the URL when it changes from elsewhere (back, another link).
  useEffect(() => {
    setText((current) => (current.trim() === (search.q ?? "") ? current : (search.q ?? "")));
  }, [search.q]);
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/api-keys",
    apiKeysInit(org, project),
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
    if (problemOf(query.error)?.status === 403) {
      return <Forbidden description="Only project admins can manage API keys." />;
    }
    throw query.error;
  }

  const rows = query.data?.pages.flatMap((page) => page.data) ?? [];
  const sorting = search.sort ? { id: search.sort, desc: search.desc === true } : undefined;
  const visible = sortRows(
    filterRows(rows, search.q, (key) => [key.name, key.prefix, key.scopes.join(" ")]),
    sorting,
    SORT_KEYS,
  );
  const openKey =
    search.open === undefined ? undefined : rows.find((key) => key.id === search.open);

  function go(
    next: (prev: typeof search) => typeof search,
    options: { replace?: boolean; ignoreBlocker?: boolean } = {},
  ) {
    void navigate({
      to: "/$org/$project/settings/api-keys",
      params: { org, project },
      search: next,
      replace: options.replace,
      ignoreBlocker: options.ignoreBlocker,
    });
  }

  function closePanel() {
    go((prev) => ({ ...prev, open: undefined }));
  }

  return (
    <SettingsListLayout
      open={search.open !== undefined}
      onClose={closePanel}
      panel={
        search.open === undefined ? null : query.isPending ? (
          <Skeleton className="m-4 h-8 w-full" />
        ) : openKey ? (
          <KeyPanel
            key={openKey.id}
            org={org}
            project={project}
            row={openKey}
            onClose={closePanel}
            onRevoked={() => {
              go((prev) => ({ ...prev, open: undefined }), { ignoreBlocker: true });
            }}
          />
        ) : (
          <SettingsPanel title="Key not found" onClose={closePanel}>
            <p className="text-body text-muted-foreground">This key is not in the loaded list.</p>
          </SettingsPanel>
        )
      }
    >
      <SettingsPage
        title="API keys"
        description="Keys let SDKs and OpenTelemetry exporters send data to this project."
        actions={
          <Button type="button" size="sm" onClick={onCreate}>
            <Plus />
            Create key
          </Button>
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
            icon={KeyRound}
            title="No API keys yet"
            description="Create a secret key to send traces."
          />
        ) : (
          <>
            <SettingsListToolbar
              label="API keys"
              query={text}
              onQueryChange={(value) => {
                setText(value);
                go((prev) => ({ ...prev, q: value.trim() || undefined }), { replace: true });
              }}
              count={`${visible.length}${query.hasNextPage ? "+" : ""} ${visible.length === 1 && !query.hasNextPage ? "key" : "keys"}`}
            />
            {visible.length === 0 ? (
              <EmptyState icon={KeyRound} title="No key matches" />
            ) : (
              <SettingsTableFrame rowCount={visible.length}>
                <DataTable
                  tableId="api-keys"
                  ariaLabel="API keys"
                  columns={COLUMNS}
                  rows={visible}
                  getRowId={(key) => key.id}
                  selectedRowId={search.open}
                  onActivate={(key) => {
                    go((prev) => ({ ...prev, open: key.id }));
                  }}
                  onSelectionMove={(key) => {
                    go((prev) => ({ ...prev, open: key.id }), { replace: true });
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

/** The row panel of one key: its details and revoke, which asks for the key's name first. */
function KeyPanel({
  org,
  project,
  row,
  onClose,
  onRevoked,
}: {
  org: string;
  project: string;
  row: ApiKey;
  onClose: () => void;
  onRevoked: () => void;
}) {
  const queryClient = useQueryClient();
  const [revokeOpen, setRevokeOpen] = useState(false);
  const revoke = $api.useMutation("delete", "/api/v1/projects/{project}/api-keys/{id}", {
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: apiKeysKey(org, project) });
      setRevokeOpen(false);
      onRevoked();
    },
  });
  const expected = row.name ?? row.prefix;
  const status = keyStatus(row, Date.now());

  return (
    <SettingsPanel
      title={row.name ?? "Unnamed key"}
      subtitle={<span className="font-mono">{row.prefix}…</span>}
      onClose={onClose}
      footer={
        <Button
          type="button"
          variant="destructive"
          onClick={() => {
            revoke.reset();
            setRevokeOpen(true);
          }}
        >
          Revoke key…
        </Button>
      }
    >
      <dl className="flex flex-col gap-3 text-body">
        <div className="flex flex-col gap-0.5">
          <dt className="text-caption text-muted-foreground">Kind</dt>
          <dd>{row.kind === "secret" ? "Secret" : "Public"}</dd>
        </div>
        <div className="flex flex-col gap-0.5">
          <dt className="text-caption text-muted-foreground">Scopes</dt>
          <dd>{row.scopes.length > 0 ? row.scopes.join(", ") : "—"}</dd>
        </div>
        <div className="flex flex-col gap-0.5">
          <dt className="text-caption text-muted-foreground">Status</dt>
          <dd>
            <StatusPill tone={status.tone}>{status.label}</StatusPill>
          </dd>
        </div>
        <div className="flex flex-col gap-0.5">
          <dt className="text-caption text-muted-foreground">Created</dt>
          <dd>
            <RelativeTime value={row.created_at} />
          </dd>
        </div>
        <div className="flex flex-col gap-0.5">
          <dt className="text-caption text-muted-foreground">Created by</dt>
          <dd>
            <CreatedBy createdBy={row.created_by} />
          </dd>
        </div>
        <div className="flex flex-col gap-0.5">
          <dt className="text-caption text-muted-foreground">Last used</dt>
          <dd>{row.last_used_at ? <RelativeTime value={row.last_used_at} /> : "Never"}</dd>
        </div>
        <div className="flex flex-col gap-0.5">
          <dt className="text-caption text-muted-foreground">Expires</dt>
          <dd>{row.expires_at ? <RelativeTime value={row.expires_at} /> : "Never"}</dd>
        </div>
      </dl>
      <TypeToConfirmDialog
        open={revokeOpen}
        onOpenChange={(next) => {
          if (!next) {
            setRevokeOpen(false);
            revoke.reset();
          }
        }}
        title="Revoke key?"
        description={
          <>
            Requests that use <span className="font-mono">{row.prefix}…</span> fail at once. This
            cannot be undone.
          </>
        }
        expected={expected}
        inputLabel={`Type "${expected}" to confirm`}
        confirmLabel="Revoke key"
        pending={revoke.isPending}
        error={revoke.error ? problemMessage(revoke.error, "Could not revoke the key.") : undefined}
        onConfirm={() => {
          revoke.mutate({ params: { path: { project: projectRef(org, project), id: row.id } } });
        }}
      />
    </SettingsPanel>
  );
}
