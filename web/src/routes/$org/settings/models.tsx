import { useQueryClient } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { Coins } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import {
  orgPricesInit,
  orgPricesKey,
  PROJECT_PRICES_PREFIX,
  PROJECT_RESOLVE_PREFIX,
} from "@/api/queries-prices";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { PriceDialog } from "@/components/prices/price-dialog";
import { PriceTable } from "@/components/prices/price-table";
import { SettingsPage } from "@/components/settings/settings-page";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { blockedByRules } from "@/lib/prices";
import { canManageOrgPrices } from "@/lib/roles";

export const Route = createFileRoute("/$org/settings/models")({
  component: OrgPricesPage,
});

function OrgPricesPage() {
  const { org } = Route.useParams();
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  const role = me.orgs.find((item) => item.slug === org)?.role;
  const canManage = canManageOrgPrices(role);
  const [adding, setAdding] = useState(false);
  return (
    <SettingsPage
      title="Model prices"
      description="Prices for every project of this organization. A project's own price beats an organization price, which beats the built-in LiteLLM prices. New prices apply to spans received from now on; stored costs are not recomputed."
      actions={
        <DisabledReason reason={canManage ? undefined : "Only admins and owners can change prices"}>
          <Button type="button" disabled={!canManage} onClick={() => setAdding(true)}>
            Add price
          </Button>
        </DisabledReason>
      }
    >
      <PriceList org={org} canManage={canManage} adding={adding} onAddingChange={setAdding} />
    </SettingsPage>
  );
}

function PriceList({
  org,
  canManage,
  adding,
  onAddingChange,
}: {
  org: string;
  canManage: boolean;
  adding: boolean;
  onAddingChange: (open: boolean) => void;
}) {
  const queryClient = useQueryClient();
  const query = $api.useQuery("get", "/api/v1/orgs/{org}/models", orgPricesInit(org));
  // Never cleared on close, so the dialogs do not blank while they animate out.
  const [editing, setEditing] = useState<Schema<"ModelPrice"> | undefined>();
  const [editOpen, setEditOpen] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<Schema<"ModelPrice"> | undefined>();
  const [deleteOpen, setDeleteOpen] = useState(false);
  const remove = $api.useMutation("delete", "/api/v1/orgs/{org}/models/{id}");

  if (query.error) {
    throw query.error;
  }

  const rows = query.data?.data ?? [];
  const scope = { kind: "org" as const, org };

  return (
    <>
      {query.isPending ? (
        <>
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </>
      ) : rows.length === 0 ? (
        <EmptyState
          icon={Coins}
          title="No organization prices"
          description="Built-in prices apply. Add a price for a model they do not cover, such as a local model."
        />
      ) : (
        <PriceTable
          label="Organization prices"
          rows={rows}
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
      <PriceDialog scope={scope} open={adding} onOpenChange={onAddingChange} />
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
            { params: { path: { org, id: deleteTarget.id } } },
            {
              onSuccess: () => {
                setDeleteOpen(false);
                remove.reset();
                void queryClient.invalidateQueries({ queryKey: orgPricesKey(org) });
                void queryClient.invalidateQueries({ queryKey: [...PROJECT_PRICES_PREFIX] });
                void queryClient.invalidateQueries({ queryKey: [...PROJECT_RESOLVE_PREFIX] });
              },
            },
          );
        }}
      />
    </>
  );
}
