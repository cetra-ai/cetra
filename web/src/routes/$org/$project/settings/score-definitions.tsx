import { useQueryClient } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { Gauge, Plus } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { scoreDefinitionsInit, scoreDefinitionsKey } from "@/api/queries-evaluation";
import type { Schema } from "@/api/types";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import {
  SCORE_CANNOT_MANAGE,
  SCORE_DATA_TYPE_LABELS,
  ScoreDefinitionForm,
} from "@/components/evaluators/score-definition-form";
import { FormError } from "@/components/form/form-error";
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
import { StatusPill } from "@/components/settings/status-pill";
import { useProject } from "@/components/shell/project-context";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { canManageEvaluation } from "@/lib/roles";
import { parseSettingsListSearch } from "@/lib/search/settings-list";

type Definition = Schema<"ScoreDefinition">;

const SORTS = ["name", "updated"] as const;

export const Route = createFileRoute("/$org/$project/settings/score-definitions")({
  validateSearch: (search) => parseSettingsListSearch(search, SORTS),
  component: ScoreDefinitionsPage,
});

/** The range of a numeric score, the labels of a categorical one, else a dash. */
function rangeOf(definition: Definition): string {
  if (definition.data_type === "categorical") {
    return definition.categories.map((category) => category.label).join(", ") || "—";
  }
  if (definition.data_type !== "numeric") {
    return "—";
  }
  const { min_value: min, max_value: max } = definition;
  if (min !== undefined && max !== undefined) {
    return `${min} – ${max}`;
  }
  if (min !== undefined) {
    return `≥ ${min}`;
  }
  if (max !== undefined) {
    return `≤ ${max}`;
  }
  return "No range";
}

/** The direction as an arrow and a word, or a dash when it is `none`. */
function directionOf(direction: Schema<"ScoreDirection">): string {
  if (direction === "higher") {
    return "↑ higher";
  }
  if (direction === "lower") {
    return "↓ lower";
  }
  return "—";
}

const SORT_KEYS: Record<string, (definition: Definition) => SortValue> = {
  name: (definition) => definition.name,
  updated: (definition) => definition.updated_at,
};

const DEFINITION_COLUMNS: DataTableColumn<Definition>[] = [
  {
    id: "name",
    size: 200,
    header: "Name",
    accessorFn: (definition) => definition.name,
    enableSorting: true,
    cell: ({ row }) => <span className="truncate">{row.original.name}</span>,
  },
  {
    id: "type",
    size: 120,
    header: "Type",
    enableSorting: false,
    cell: ({ row }) => SCORE_DATA_TYPE_LABELS[row.original.data_type],
  },
  {
    id: "range",
    size: 200,
    header: "Range or categories",
    enableSorting: false,
    meta: { grow: true },
    cell: ({ row }) => <span className="block truncate">{rangeOf(row.original)}</span>,
  },
  {
    id: "direction",
    size: 120,
    header: "Direction",
    enableSorting: false,
    cell: ({ row }) => directionOf(row.original.direction),
  },
  {
    id: "status",
    size: 104,
    header: "Status",
    enableSorting: false,
    cell: ({ row }) =>
      row.original.archived_at ? (
        <StatusPill tone="unset">Archived</StatusPill>
      ) : (
        <StatusPill tone="ok">Active</StatusPill>
      ),
  },
  {
    id: "updated",
    size: 112,
    header: "Updated",
    accessorFn: (definition) => definition.updated_at,
    enableSorting: true,
    cell: ({ row }) => <RelativeTime value={row.original.updated_at} />,
  },
];

function ScoreDefinitionsPage() {
  const { org, project, ref, data } = useProject();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const canManage = canManageEvaluation(data.role);
  const [includeArchived, setIncludeArchived] = useState(false);
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/score-definitions",
    scoreDefinitionsInit(org, project, includeArchived),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );

  if (query.error) {
    throw query.error;
  }

  const rows = query.data?.pages.flatMap((page) => page.data) ?? [];
  const sorting =
    search.sort === undefined ? undefined : { id: search.sort, desc: search.desc === true };
  const shown = sortRows(
    filterRows(rows, search.q, (definition) => [
      definition.name,
      definition.description,
      SCORE_DATA_TYPE_LABELS[definition.data_type],
    ]),
    sorting,
    SORT_KEYS,
  );
  const openId = search.open;
  const openDefinition =
    openId === undefined || openId === "new"
      ? undefined
      : rows.find((definition) => definition.id === openId);

  function openPanel(id: string, replace = false) {
    void navigate({
      to: "/$org/$project/settings/score-definitions",
      params: { org, project },
      search: (prev) => ({ ...prev, open: id }),
      replace,
    });
  }

  // Closing asks first when the form is dirty, unless the save or archive already went through.
  function closePanel(ignoreBlocker = false) {
    void navigate({
      to: "/$org/$project/settings/score-definitions",
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
            title="Add score definition"
            subtitle="Annotations and evaluators must fit the definition of their score name."
            onClose={() => {
              closePanel();
            }}
          >
            {canManage ? (
              <ScoreDefinitionForm
                org={org}
                project={project}
                definition={undefined}
                canManage={canManage}
                onDone={() => {
                  closePanel(true);
                }}
              />
            ) : (
              <p className="text-body text-muted-foreground">{SCORE_CANNOT_MANAGE}</p>
            )}
          </SettingsPanel>
        ) : openDefinition ? (
          <DefinitionPanel
            key={openDefinition.id}
            org={org}
            project={project}
            projectPath={ref}
            definition={openDefinition}
            canManage={canManage}
            onClose={() => {
              closePanel();
            }}
            onDone={() => {
              closePanel(true);
            }}
          />
        ) : (
          <SettingsPanel
            title={query.isPending ? "Loading definition" : "Definition not found"}
            onClose={() => {
              closePanel();
            }}
          >
            {query.isPending ? (
              <Skeleton className="h-8 w-full" />
            ) : (
              <p className="text-body text-muted-foreground">
                This definition is not in the list any more.
              </p>
            )}
          </SettingsPanel>
        )
      }
    >
      <SettingsPage
        title="Score definitions"
        description="Named score shapes. Annotations and evaluators must fit them."
        actions={
          <DisabledReason reason={canManage ? undefined : SCORE_CANNOT_MANAGE}>
            <Button
              type="button"
              disabled={!canManage}
              onClick={() => {
                openPanel("new");
              }}
            >
              <Plus />
              Add definition
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
        ) : (
          <>
            <SettingsListToolbar
              label="score definitions"
              query={search.q ?? ""}
              onQueryChange={(next) => {
                void navigate({
                  to: "/$org/$project/settings/score-definitions",
                  params: { org, project },
                  search: (prev) => ({ ...prev, q: next.trim() === "" ? undefined : next }),
                  replace: true,
                  ignoreBlocker: true,
                });
              }}
              count={
                search.q
                  ? `${shown.length} of ${rows.length} definitions`
                  : `${rows.length} ${rows.length === 1 ? "definition" : "definitions"}`
              }
            >
              <div className="flex items-center gap-2">
                <Switch
                  id="show-archived"
                  size="sm"
                  checked={includeArchived}
                  onCheckedChange={setIncludeArchived}
                />
                <Label htmlFor="show-archived">Show archived</Label>
              </div>
            </SettingsListToolbar>
            {rows.length === 0 ? (
              <EmptyState
                icon={Gauge}
                title="No score definitions yet"
                description="Add one to check score writes and evaluator outputs against its type and range."
              />
            ) : (
              <SettingsTableFrame rowCount={shown.length}>
                <DataTable
                  tableId="score-definitions"
                  ariaLabel="Score definitions"
                  columns={DEFINITION_COLUMNS}
                  rows={shown}
                  getRowId={(row) => row.id}
                  selectedRowId={openId}
                  onActivate={(row) => {
                    openPanel(row.id);
                  }}
                  onSelectionMove={(row) => {
                    openPanel(row.id, true);
                  }}
                  hasNextPage={query.hasNextPage}
                  isFetchingNextPage={query.isFetchingNextPage}
                  fetchNextPage={() => {
                    void query.fetchNextPage();
                  }}
                  sorting={sorting}
                  onSortingChange={(next) => {
                    void navigate({
                      to: "/$org/$project/settings/score-definitions",
                      params: { org, project },
                      search: (prev) => ({
                        ...prev,
                        sort: next?.id,
                        desc: next?.desc ? true : undefined,
                      }),
                      replace: true,
                      ignoreBlocker: true,
                    });
                  }}
                  emptyState={<EmptyState icon={Gauge} title="No definition matches" />}
                />
              </SettingsTableFrame>
            )}
          </>
        )}
      </SettingsPage>
    </SettingsListLayout>
  );
}

/** The panel of an existing definition: its form, and archive (or restore) in the footer. */
function DefinitionPanel({
  org,
  project,
  projectPath,
  definition,
  canManage,
  onClose,
  onDone,
}: {
  org: string;
  project: string;
  projectPath: string;
  definition: Definition;
  canManage: boolean;
  onClose: () => void;
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const [archiveOpen, setArchiveOpen] = useState(false);
  const archive = $api.useMutation("delete", "/api/v1/projects/{project}/score-definitions/{id}");
  const restore = $api.useMutation("patch", "/api/v1/projects/{project}/score-definitions/{id}");
  const archived = definition.archived_at !== undefined;
  const reason = canManage ? undefined : SCORE_CANNOT_MANAGE;

  function refresh() {
    void queryClient.invalidateQueries({ queryKey: scoreDefinitionsKey(org, project) });
  }

  return (
    <SettingsPanel
      title={definition.name}
      subtitle={SCORE_DATA_TYPE_LABELS[definition.data_type]}
      onClose={onClose}
      footer={
        archived ? (
          <DisabledReason reason={reason}>
            <Button
              type="button"
              variant="outline"
              disabled={!canManage || restore.isPending}
              onClick={() => {
                restore.mutate(
                  {
                    params: { path: { project: projectPath, id: definition.id } },
                    body: { archived: false },
                  },
                  { onSuccess: refresh },
                );
              }}
            >
              Restore
            </Button>
          </DisabledReason>
        ) : (
          <DisabledReason reason={reason}>
            <Button
              type="button"
              variant="outline"
              className="text-destructive"
              disabled={!canManage}
              onClick={() => {
                setArchiveOpen(true);
              }}
            >
              Archive
            </Button>
          </DisabledReason>
        )
      }
    >
      <ScoreDefinitionForm
        org={org}
        project={project}
        definition={definition}
        canManage={canManage}
        onDone={onDone}
      />
      <FormError
        message={
          restore.error
            ? problemMessage(restore.error, "Could not restore the definition.")
            : undefined
        }
      />
      <TypeToConfirmDialog
        open={archiveOpen}
        onOpenChange={(next) => {
          setArchiveOpen(next);
          if (!next) {
            archive.reset();
          }
        }}
        title={`Archive ${definition.name}?`}
        description="Scores keep their values; new scores are no longer checked against it."
        expected={definition.name}
        inputLabel={`Type "${definition.name}" to confirm`}
        confirmLabel="Archive"
        pending={archive.isPending}
        error={
          archive.error
            ? problemMessage(archive.error, "Could not archive the definition.")
            : undefined
        }
        onConfirm={() => {
          archive.mutate(
            { params: { path: { project: projectPath, id: definition.id } } },
            {
              onSuccess: () => {
                setArchiveOpen(false);
                archive.reset();
                refresh();
                onDone();
              },
            },
          );
        }}
      />
    </SettingsPanel>
  );
}
