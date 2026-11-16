import { useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { Coins, Loader2 } from "lucide-react";
import { useEffect, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import {
  PROJECT_PRICES_PREFIX,
  PROJECT_RESOLVE_PREFIX,
  projectPricesInit,
} from "@/api/queries-prices";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { PriceDialog, type PriceScope } from "@/components/prices/price-dialog";
import { PriceResolver } from "@/components/prices/price-resolver";
import { PriceTable } from "@/components/prices/price-table";
import { SettingsPage } from "@/components/settings/settings-page";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { blockedByRules } from "@/lib/prices";
import { atLeast } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/settings/models")({
  component: ProjectPricesPage,
});

const NO_PERMISSION = "Only project admins can change prices";

function ProjectPricesPage() {
  const { org, project, ref, data } = useProject();
  const canManage = atLeast(data.role, "admin");
  const queryClient = useQueryClient();
  const scope: PriceScope = { kind: "project", org, project };
  const [adding, setAdding] = useState(false);
  // Never cleared on close, so the dialog does not blank while it animates out.
  const [editing, setEditing] = useState<Schema<"ModelPrice"> | undefined>();
  const [editOpen, setEditOpen] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<Schema<"ModelPrice"> | undefined>();
  const [deleteOpen, setDeleteOpen] = useState(false);
  const [search, setSearch] = useState("");
  const [term, setTerm] = useState("");
  const remove = $api.useMutation("delete", "/api/v1/projects/{project}/models/{id}");

  useEffect(() => {
    const timer = setTimeout(() => {
      setTerm(search.trim());
    }, 250);
    return () => {
      clearTimeout(timer);
    };
  }, [search]);

  const own = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/models",
    projectPricesInit(org, project, "project", ""),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  const inherited = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/models",
    projectPricesInit(org, project, "org", ""),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  const builtin = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/models",
    projectPricesInit(org, project, "builtin", term),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      enabled: term.length >= 2,
    },
  );

  if (own.error) {
    throw own.error;
  }
  if (inherited.error) {
    throw inherited.error;
  }
  if (builtin.error) {
    throw builtin.error;
  }

  const ownRows = own.data?.pages.flatMap((page) => page.data) ?? [];
  const orgRows = inherited.data?.pages.flatMap((page) => page.data) ?? [];
  const builtinRows = builtin.data?.pages.flatMap((page) => page.data) ?? [];

  function refresh() {
    void queryClient.invalidateQueries({ queryKey: [...PROJECT_PRICES_PREFIX] });
    void queryClient.invalidateQueries({ queryKey: [...PROJECT_RESOLVE_PREFIX] });
  }

  return (
    <SettingsPage
      title="Model prices"
      description="What costs are computed with in this project. A project price beats an organization price, which beats the built-in LiteLLM prices. New prices apply to spans received from now on; stored costs are not recomputed."
      actions={
        <DisabledReason reason={canManage ? undefined : NO_PERMISSION}>
          <Button
            type="button"
            disabled={!canManage}
            onClick={() => {
              setAdding(true);
            }}
          >
            Add price
          </Button>
        </DisabledReason>
      }
    >
      <section aria-labelledby="price-resolver-heading" className="flex flex-col gap-3">
        <h2 id="price-resolver-heading" className="text-base font-medium">
          Which price applies?
        </h2>
        <PriceResolver org={org} project={project} />
      </section>

      <section aria-labelledby="project-prices-heading" className="flex flex-col gap-3">
        <h2 id="project-prices-heading" className="text-base font-medium">
          Project prices
        </h2>
        {own.isPending ? (
          <>
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
          </>
        ) : ownRows.length === 0 ? (
          <EmptyState
            icon={Coins}
            title="No project prices"
            description="Organization and built-in prices apply."
          />
        ) : (
          <PriceTable
            label="Project prices"
            rows={ownRows}
            actions={
              canManage
                ? (row) => (
                    <div className="flex items-center justify-end gap-1">
                      <Button
                        type="button"
                        variant="ghost"
                        size="sm"
                        onClick={() => {
                          setEditing(row);
                          setEditOpen(true);
                        }}
                      >
                        Edit
                      </Button>
                      <Button
                        type="button"
                        variant="ghost"
                        size="sm"
                        onClick={() => {
                          setDeleteTarget(row);
                          setDeleteOpen(true);
                        }}
                      >
                        Delete
                      </Button>
                    </div>
                  )
                : undefined
            }
          />
        )}
        {own.hasNextPage ? (
          <Button
            type="button"
            variant="outline"
            className="w-fit"
            disabled={own.isFetchingNextPage}
            onClick={() => {
              void own.fetchNextPage();
            }}
          >
            {own.isFetchingNextPage ? <Loader2 className="animate-spin" /> : null}
            Load more
          </Button>
        ) : null}
      </section>

      <section aria-labelledby="org-prices-heading" className="flex flex-col gap-3">
        <div className="flex flex-wrap items-baseline justify-between gap-2">
          <h2 id="org-prices-heading" className="text-base font-medium">
            Organization prices
          </h2>
          <Link to="/$org/settings/models" params={{ org }} className="text-sm underline">
            Manage organization prices
          </Link>
        </div>
        {inherited.isPending ? (
          <Skeleton className="h-8 w-full" />
        ) : orgRows.length === 0 ? (
          <p className="text-sm text-muted-foreground">No organization prices.</p>
        ) : (
          <PriceTable label="Organization prices" rows={orgRows} />
        )}
      </section>

      <section aria-labelledby="builtin-prices-heading" className="flex flex-col gap-3">
        <h2 id="builtin-prices-heading" className="text-base font-medium">
          Built-in prices
        </h2>
        <Input
          aria-label="Search built-in prices"
          placeholder="Search by model, e.g. gpt-4o"
          value={search}
          onChange={(event) => {
            setSearch(event.target.value);
          }}
        />
        {term.length < 2 ? (
          <p className="text-sm text-muted-foreground">Search the built-in prices from LiteLLM.</p>
        ) : builtin.isPending ? (
          <Skeleton className="h-8 w-full" />
        ) : builtinRows.length === 0 ? (
          <p className="text-sm text-muted-foreground">No built-in prices match.</p>
        ) : (
          <PriceTable label="Built-in prices" rows={builtinRows} />
        )}
        {term.length >= 2 && builtin.hasNextPage ? (
          <Button
            type="button"
            variant="outline"
            className="w-fit"
            disabled={builtin.isFetchingNextPage}
            onClick={() => {
              void builtin.fetchNextPage();
            }}
          >
            {builtin.isFetchingNextPage ? <Loader2 className="animate-spin" /> : null}
            Load more
          </Button>
        ) : null}
      </section>

      <PriceDialog scope={scope} open={adding} onOpenChange={setAdding} />
      <PriceDialog scope={scope} row={editing} open={editOpen} onOpenChange={setEditOpen} />
      <ConfirmDialog
        open={deleteOpen}
        onOpenChange={(open) => {
          if (!open) {
            setDeleteOpen(false);
            remove.reset();
          }
        }}
        title="Delete this price?"
        description="Spans received after this use the next matching price. Stored costs do not change."
        confirmLabel="Delete"
        destructive
        pending={remove.isPending}
        error={
          remove.error
            ? (blockedByRules(remove.error) ??
              problemMessage(remove.error, "Could not delete the price."))
            : undefined
        }
        onConfirm={() => {
          if (!deleteTarget?.id) {
            return;
          }
          remove.mutate(
            { params: { path: { project: ref, id: deleteTarget.id } } },
            {
              onSuccess: () => {
                setDeleteOpen(false);
                remove.reset();
                refresh();
              },
            },
          );
        }}
      />
    </SettingsPage>
  );
}
