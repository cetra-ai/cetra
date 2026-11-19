import { useQueryClient } from "@tanstack/react-query";
import { KeyRound, Loader2, MoreHorizontal, Plus } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { personalTokensInit, personalTokensKey } from "@/api/queries-account";
import type { Schema } from "@/api/types";
import { CreateTokenDialog } from "@/components/account/create-token-dialog";
import { accessLabel, tokenStatus } from "@/components/account/token-format";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { EmptyState } from "@/components/empty-state";
import { FormError } from "@/components/form/form-error";
import { SecretDialog } from "@/components/secret-dialog";
import { SettingsCard } from "@/components/settings/settings-card";
import {
  filterRows,
  SettingsListToolbar,
  SettingsTableFrame,
  type SortValue,
  sortRows,
} from "@/components/settings/settings-list";
import { StatusPill } from "@/components/settings/status-pill";
import { TimeText } from "@/components/time-text";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Skeleton } from "@/components/ui/skeleton";
import type { SettingsListSearch } from "@/lib/search/settings-list";

type Token = Schema<"PersonalToken">;

const SORT_KEYS: Record<string, (token: Token) => SortValue> = {
  name: (token) => token.name,
  created: (token) => Date.parse(token.created_at),
  "last-used": (token) => (token.last_used_at ? Date.parse(token.last_used_at) : undefined),
  expires: (token) => (token.expires_at ? Date.parse(token.expires_at) : undefined),
};

/** The sort columns the token list accepts in the URL. */
export const PERSONAL_TOKEN_SORTS = Object.keys(SORT_KEYS);

function tokenColumns(onRevoke: (token: Token) => void): DataTableColumn<Token>[] {
  return [
    {
      id: "name",
      size: 200,
      header: "Name",
      enableSorting: true,
      accessorFn: (row) => row.name,
      cell: ({ row }) => <span className="block truncate">{row.original.name}</span>,
    },
    {
      id: "prefix",
      size: 160,
      header: "Prefix",
      enableSorting: false,
      cell: ({ row }) => <span className="font-mono">{row.original.prefix}</span>,
    },
    {
      id: "access",
      size: 128,
      header: "Access",
      enableSorting: false,
      cell: ({ row }) => accessLabel(row.original.scopes),
    },
    {
      id: "status",
      size: 136,
      header: "Status",
      enableSorting: false,
      cell: ({ row }) => {
        const status = tokenStatus(row.original.expires_at, Date.now());
        return <StatusPill tone={status.tone}>{status.label}</StatusPill>;
      },
    },
    {
      id: "created",
      size: 176,
      header: "Created",
      enableSorting: true,
      accessorFn: (row) => Date.parse(row.created_at),
      cell: ({ row }) => <TimeText value={row.original.created_at} style="datetime" />,
    },
    {
      id: "last-used",
      size: 176,
      header: "Last used",
      enableSorting: true,
      accessorFn: (row) => (row.last_used_at ? Date.parse(row.last_used_at) : undefined),
      cell: ({ row }) =>
        row.original.last_used_at ? (
          <TimeText value={row.original.last_used_at} style="datetime" />
        ) : (
          "Never"
        ),
    },
    {
      id: "expires",
      size: 176,
      header: "Expires",
      enableSorting: true,
      accessorFn: (row) => (row.expires_at ? Date.parse(row.expires_at) : undefined),
      cell: ({ row }) =>
        row.original.expires_at ? (
          <TimeText value={row.original.expires_at} style="datetime" />
        ) : (
          "Never"
        ),
    },
    {
      id: "actions",
      size: 56,
      header: "Actions",
      enableSorting: false,
      meta: { align: "right" },
      cell: ({ row }) => (
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button variant="ghost" size="icon-xs" aria-label={`Actions for ${row.original.name}`}>
              <MoreHorizontal />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem
              className="text-destructive"
              onSelect={() => {
                onRevoke(row.original);
              }}
            >
              Revoke
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      ),
    },
  ];
}

/** The signed-in user's personal access tokens: search and sort live in the account URL. */
export function PersonalTokens({
  search,
  onSearchChange,
}: {
  search: SettingsListSearch;
  onSearchChange: (patch: Partial<SettingsListSearch>) => void;
}) {
  const queryClient = useQueryClient();
  const tokens = $api.useInfiniteQuery("get", "/api/v1/me/tokens", personalTokensInit(), {
    pageParamName: "cursor",
    initialPageParam: null,
    getNextPageParam: (page) => page.next_cursor ?? undefined,
  });
  const revoke = $api.useMutation("delete", "/api/v1/me/tokens/{id}");
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<Schema<"CreatedPersonalToken"> | undefined>();
  const [revoking, setRevoking] = useState<Token | undefined>();
  // The input keeps the typed text (spaces included); the URL gets it trimmed.
  const [text, setText] = useState(search.q ?? "");
  const columns = useMemo(() => tokenColumns(setRevoking), []);

  // The text follows the URL when it changes from elsewhere (back, another link).
  useEffect(() => {
    setText((current) => (current.trim() === (search.q ?? "") ? current : (search.q ?? "")));
  }, [search.q]);

  // A failed first page has no data to show: the route's ErrorState renders it. A failed
  // "Load more" keeps the rows already loaded, so only an inline error is added.
  if (tokens.error && tokens.data === undefined) {
    throw tokens.error;
  }

  const loaded = tokens.data?.pages.flatMap((page) => page.data) ?? [];
  const shown = sortRows(
    filterRows(loaded, search.q, (token) => [token.name, token.prefix, accessLabel(token.scopes)]),
    search.sort === undefined ? undefined : { id: search.sort, desc: search.desc === true },
    SORT_KEYS,
  );
  const count =
    search.q === undefined
      ? `${loaded.length}${tokens.hasNextPage ? "+" : ""} tokens`
      : `${shown.length} of ${loaded.length} loaded`;

  return (
    <>
      <SettingsCard
        title="Personal access tokens"
        description="Tokens let the CLI and MCP clients act as you. Treat them like passwords."
        actions={
          <Button
            type="button"
            size="sm"
            onClick={() => {
              setCreating(true);
            }}
          >
            <Plus aria-hidden="true" />
            New token
          </Button>
        }
      >
        {tokens.isPending ? (
          <>
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
          </>
        ) : loaded.length === 0 ? (
          <EmptyState
            icon={KeyRound}
            title="No personal access tokens"
            description="Create one to use the CLI or an MCP client."
          />
        ) : (
          <>
            <SettingsListToolbar
              label="tokens"
              query={text}
              count={count}
              onQueryChange={(next) => {
                setText(next);
                onSearchChange({ q: next.trim() === "" ? undefined : next.trim() });
              }}
            />
            <SettingsTableFrame rowCount={shown.length}>
              {shown.length === 0 ? (
                <p className="p-3 text-body text-muted-foreground">No token matches.</p>
              ) : (
                <DataTable
                  tableId="personal-tokens"
                  ariaLabel="Personal access tokens"
                  columns={columns}
                  rows={shown}
                  getRowId={(token) => token.id}
                  // The rows have no panel, so activating one does nothing.
                  onActivate={() => undefined}
                  hasNextPage={false}
                  isFetchingNextPage={false}
                  fetchNextPage={() => undefined}
                  sorting={
                    search.sort === undefined
                      ? undefined
                      : { id: search.sort, desc: search.desc === true }
                  }
                  onSortingChange={(next) => {
                    onSearchChange({
                      sort: next?.id,
                      desc: next?.desc === true ? true : undefined,
                    });
                  }}
                  emptyState={null}
                />
              )}
            </SettingsTableFrame>
            {tokens.hasNextPage ? (
              <Button
                type="button"
                variant="outline"
                className="w-fit"
                disabled={tokens.isFetchingNextPage}
                onClick={() => {
                  void tokens.fetchNextPage();
                }}
              >
                {tokens.isFetchingNextPage ? <Loader2 className="animate-spin" /> : null}
                Load more
              </Button>
            ) : null}
            <FormError
              message={
                tokens.error && tokens.data !== undefined
                  ? "Could not load more tokens."
                  : undefined
              }
            />
          </>
        )}
      </SettingsCard>
      <CreateTokenDialog
        open={creating}
        onOpenChange={setCreating}
        onCreated={(data) => {
          setCreated(data);
          void queryClient.invalidateQueries({ queryKey: personalTokensKey() });
        }}
      />
      {created ? (
        <SecretDialog
          open
          title="Personal access token created"
          secret={created.secret}
          description="Use it as a bearer token in the Authorization header."
          onDone={() => {
            setCreated(undefined);
          }}
        />
      ) : null}
      <TypeToConfirmDialog
        open={revoking !== undefined}
        onOpenChange={(open) => {
          if (!open) {
            setRevoking(undefined);
            revoke.reset();
          }
        }}
        title="Revoke token?"
        description={
          <>
            Anything using <strong>{revoking?.name}</strong> (
            <span className="font-mono">{revoking?.prefix}</span>) stops working at once.
          </>
        }
        expected={revoking?.name ?? ""}
        inputLabel={`Type "${revoking?.name ?? ""}" to confirm`}
        confirmLabel="Revoke"
        pending={revoke.isPending}
        error={
          revoke.error
            ? problemMessage(revoke.error, "Could not revoke the token. Try again.")
            : undefined
        }
        onConfirm={() => {
          if (!revoking) {
            return;
          }
          revoke.mutate(
            { params: { path: { id: revoking.id } } },
            {
              onSuccess: () => {
                setRevoking(undefined);
                revoke.reset();
                void queryClient.invalidateQueries({ queryKey: personalTokensKey() });
              },
              onError: (error) => {
                // A 404 means the token is already gone: treat it as revoked.
                if (problemOf(error)?.status === 404) {
                  setRevoking(undefined);
                  revoke.reset();
                  void queryClient.invalidateQueries({ queryKey: personalTokensKey() });
                }
              },
            },
          );
        }}
      />
    </>
  );
}
