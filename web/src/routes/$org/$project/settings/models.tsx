import { keepPreviousData } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { Coins } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import { $api } from "@/api/client";
import { projectPricesInit } from "@/api/queries-prices";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { PriceFormPanel, type PriceScope } from "@/components/prices/price-form";
import { PriceResolver } from "@/components/prices/price-resolver";
import { PriceTable } from "@/components/prices/price-table";
import { useSearchText } from "@/components/prices/use-search-text";
import { SettingsCard } from "@/components/settings/settings-card";
import {
  SettingsListLayout,
  SettingsListToolbar,
  SettingsPanel,
} from "@/components/settings/settings-list";
import { SettingsPage } from "@/components/settings/settings-page";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { atLeast } from "@/lib/roles";
import { parseSettingsListSearch } from "@/lib/search/settings-list";

export const Route = createFileRoute("/$org/$project/settings/models")({
  validateSearch: (search) => parseSettingsListSearch(search, []),
  component: ProjectPricesPage,
});

const NO_PERMISSION = "Only project admins can change prices";
/** The `open` value that opens the panel to add a price. */
const NEW = "new";

function ProjectPricesPage() {
  const { org, project, data } = useProject();
  const canManage = atLeast(data.role, "admin");
  const { q, open } = Route.useSearch();
  const navigate = Route.useNavigate();
  const scope: PriceScope = { kind: "project", org, project };

  const [text, setText] = useSearchText(q, (next) => {
    void navigate({
      to: "/$org/$project/settings/models",
      params: { org, project },
      search: (prev) => ({ ...prev, q: next }),
      replace: true,
    });
  });
  const [builtinText, setBuiltinText] = useState("");
  const [builtinTerm, setBuiltinTerm] = useState("");

  useEffect(() => {
    const timer = setTimeout(() => {
      setBuiltinTerm(builtinText.trim());
    }, 250);
    return () => {
      clearTimeout(timer);
    };
  }, [builtinText]);

  const own = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/models",
    projectPricesInit(org, project, "project", q ?? ""),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      placeholderData: keepPreviousData,
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
    projectPricesInit(org, project, "builtin", builtinTerm),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      enabled: builtinTerm.length >= 2,
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

  function setOpen(next: string | undefined, ignoreBlocker = false) {
    void navigate({
      to: "/$org/$project/settings/models",
      params: { org, project },
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
    const row = open === NEW ? undefined : ownRows.find((item) => item.id === open);
    if (open === NEW || row !== undefined) {
      return (
        <PriceFormPanel
          key={open}
          scope={scope}
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
        {own.isPending ? (
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
        title="Model prices"
        description="What costs are computed with in this project. A project price beats an organization price, which beats the built-in LiteLLM prices. New prices apply to spans received from now on; stored costs are not recomputed."
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
        <SettingsCard title="Project prices">
          <SettingsListToolbar label="project prices" query={text} onQueryChange={setText} />
          {own.isPending ? (
            <>
              <Skeleton className="h-8 w-full" />
              <Skeleton className="h-8 w-full" />
            </>
          ) : ownRows.length === 0 ? (
            q === undefined ? (
              <EmptyState
                icon={Coins}
                title="No project prices"
                description="Organization and built-in prices apply."
              />
            ) : (
              <p className="text-body text-muted-foreground">No project prices match.</p>
            )
          ) : (
            <PriceTable
              label="Project prices"
              rows={ownRows}
              selectedRowId={open}
              onActivate={(row) => {
                if (row.id !== undefined) {
                  setOpen(row.id);
                }
              }}
              more={own}
            />
          )}
        </SettingsCard>

        <SettingsCard
          title="Organization prices"
          actions={
            <Link to="/$org/settings/models" params={{ org }} className="text-body underline">
              Manage organization prices
            </Link>
          }
        >
          {inherited.isPending ? (
            <Skeleton className="h-8 w-full" />
          ) : orgRows.length === 0 ? (
            <p className="text-body text-muted-foreground">No organization prices.</p>
          ) : (
            <PriceTable label="Organization prices" rows={orgRows} more={inherited} />
          )}
        </SettingsCard>

        <SettingsCard title="Built-in prices">
          <SettingsListToolbar
            label="built-in prices"
            query={builtinText}
            onQueryChange={setBuiltinText}
          />
          {builtinTerm.length < 2 ? (
            <p className="text-body text-muted-foreground">
              Search the built-in prices from LiteLLM.
            </p>
          ) : builtin.isPending ? (
            <Skeleton className="h-8 w-full" />
          ) : builtinRows.length === 0 ? (
            <p className="text-body text-muted-foreground">No built-in prices match.</p>
          ) : (
            <PriceTable label="Built-in prices" rows={builtinRows} more={builtin} />
          )}
        </SettingsCard>

        <SettingsCard title="Which price applies?">
          <PriceResolver org={org} project={project} />
        </SettingsCard>
      </SettingsPage>
    </SettingsListLayout>
  );
}
