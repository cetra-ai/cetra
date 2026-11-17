import { createFileRoute } from "@tanstack/react-router";
import { Coins } from "lucide-react";
import type { ReactNode } from "react";
import { $api } from "@/api/client";
import { orgPricesInit } from "@/api/queries-prices";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { PriceFormPanel } from "@/components/prices/price-form";
import { type PriceSorting, PriceTable } from "@/components/prices/price-table";
import { useSearchText } from "@/components/prices/use-search-text";
import { SettingsCard } from "@/components/settings/settings-card";
import {
  filterRows,
  SettingsListLayout,
  SettingsListToolbar,
  SettingsPanel,
  type SortValue,
  sortRows,
} from "@/components/settings/settings-list";
import { SettingsPage } from "@/components/settings/settings-page";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { canManageOrgPrices } from "@/lib/roles";
import { parseSettingsListSearch } from "@/lib/search/settings-list";

export const Route = createFileRoute("/$org/settings/models")({
  validateSearch: (search) => parseSettingsListSearch(search, ["model", "input", "output"]),
  component: OrgPricesPage,
});

const NO_PERMISSION = "Only admins and owners can change prices";
/** The `open` value that opens the panel to add a price. */
const NEW = "new";

type PriceRow = Schema<"ModelPrice">;

/** The sort keys of the price columns, by column id. */
const SORT_KEYS: Record<string, (row: PriceRow) => SortValue> = {
  model: (row) => row.model_name,
  input: (row) => row.price_input,
  output: (row) => row.price_output,
};

function OrgPricesPage() {
  const { org } = Route.useParams();
  const { q, sort, desc, open } = Route.useSearch();
  const navigate = Route.useNavigate();
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  const role = me.orgs.find((item) => item.slug === org)?.role;
  const canManage = canManageOrgPrices(role);
  const query = $api.useQuery("get", "/api/v1/orgs/{org}/models", orgPricesInit(org));
  const [text, setText] = useSearchText(q, (next) => {
    void navigate({
      to: "/$org/settings/models",
      params: { org },
      search: (prev) => ({ ...prev, q: next }),
      replace: true,
    });
  });

  if (query.error) {
    throw query.error;
  }

  const rows = query.data?.data ?? [];
  const sorting: PriceSorting | undefined =
    sort === undefined ? undefined : { id: sort, desc: desc === true };
  const shown = sortRows(
    filterRows(rows, q, (row) => [row.model_name, row.match_pattern, row.provider]),
    sorting,
    SORT_KEYS,
  );

  function setOpen(next: string | undefined, ignoreBlocker = false) {
    void navigate({
      to: "/$org/settings/models",
      params: { org },
      search: (prev) => ({ ...prev, open: next }),
      ignoreBlocker,
    });
  }

  const close = () => {
    setOpen(undefined);
  };

  function panel(): ReactNode {
    if (open === undefined) {
      return null;
    }
    const row = open === NEW ? undefined : rows.find((item) => item.id === open);
    if (open === NEW || row !== undefined) {
      return (
        <PriceFormPanel
          key={open}
          scope={{ kind: "org", org }}
          row={row}
          readOnlyReason={canManage ? undefined : NO_PERMISSION}
          onClose={close}
          onDone={() => {
            setOpen(undefined, true);
          }}
        />
      );
    }
    return (
      <SettingsPanel title="Edit model price" onClose={close}>
        {query.isPending ? (
          <Skeleton className="h-8 w-full" />
        ) : (
          <p className="text-body text-muted-foreground">Price not found.</p>
        )}
      </SettingsPanel>
    );
  }

  return (
    <SettingsListLayout open={open !== undefined} onClose={close} panel={panel()}>
      <SettingsPage
        width="wide"
        title="Model prices"
        description="Default prices for every project in the organization. A project can override them in its own Model prices. New prices apply to spans received from now on; stored costs are not recomputed."
        actions={
          <DisabledReason reason={canManage ? undefined : NO_PERMISSION}>
            <Button
              type="button"
              disabled={!canManage}
              onClick={() => {
                setOpen(NEW);
              }}
            >
              Add price
            </Button>
          </DisabledReason>
        }
      >
        <SettingsCard title="Organization prices">
          <SettingsListToolbar label="organization prices" query={text} onQueryChange={setText} />
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
          ) : shown.length === 0 ? (
            <p className="text-body text-muted-foreground">No organization prices match.</p>
          ) : (
            <PriceTable
              label="Organization prices"
              rows={shown}
              selectedRowId={open}
              onActivate={(row) => {
                if (row.id !== undefined) {
                  setOpen(row.id);
                }
              }}
              sorting={sorting}
              onSortingChange={(next) => {
                void navigate({
                  to: "/$org/settings/models",
                  params: { org },
                  search: (prev) => ({
                    ...prev,
                    sort: next?.id,
                    desc: next?.desc ? true : undefined,
                  }),
                  replace: true,
                });
              }}
            />
          )}
        </SettingsCard>
      </SettingsPage>
    </SettingsListLayout>
  );
}
