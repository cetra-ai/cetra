import { useQueryClient } from "@tanstack/react-query";
import { Search, Table2 } from "lucide-react";
import { useMemo, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { datasetItemsInit, datasetWriteKeys } from "@/api/queries-datasets";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { DataTable } from "@/components/data-table/data-table";
import { DatasetItemsEmpty } from "@/components/datasets/datasets-empty";
import { type CellEdit, ItemEditContext, itemColumns } from "@/components/datasets/item-columns";
import { ItemPanel } from "@/components/datasets/item-panel";
import {
  type CellErrors,
  changedFields,
  type Drafts,
  draftedValue,
  EditBar,
  editBody,
  editErrorsOf,
  hasDraft,
  omitKeys,
  SelectionBar,
  withDraft,
} from "@/components/datasets/items-edit";
import { ItemSearch } from "@/components/datasets/items-toolbar";
import { EmptyState } from "@/components/empty-state";
import { TraceLayoutControls } from "@/components/layout-switcher";
import { useProject } from "@/components/shell/project-context";
import { Filmstrip } from "@/components/trace/filmstrip";
import { SplitPane } from "@/components/trace/split-pane";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { itemShortId } from "@/lib/datasets";
import { useListLayout } from "@/lib/list-layout";

const FALLBACK_SAVE = "The items could not be saved";
const FALLBACK_DELETE = "The items could not be deleted";

/**
 * The items of one version of a dataset, a page at a time, beside the item panel. The open item
 * is the page's `item` search param; `onOpenItem` changes it. `search` is the applied item search
 * text (the page's `q` search param), and `onSearch` applies a new one.
 *
 * On the latest version the rows have checkboxes (delete them in one version), and an Edit toggle
 * makes the Input, Expected output and Metadata cells editable; the changes are saved in one
 * version.
 */
export function ItemsTable({
  datasetId,
  version,
  latest,
  editable,
  openItemId,
  onOpenItem,
  onItemDirtyChange,
  onAddItem,
  onImport,
  search,
  onSearch,
}: {
  datasetId: string;
  version: number;
  /** Whether `version` is the latest one. */
  latest: boolean;
  /** Whether the item panel can edit: the latest version, and the user may manage datasets. */
  editable: boolean;
  openItemId?: string;
  onOpenItem: (id: string | undefined) => void;
  /** The item panel's form has typed-in changes. */
  onItemDirtyChange: (dirty: boolean) => void;
  onAddItem: () => void;
  onImport: () => void;
  search?: string;
  onSearch: (text: string | undefined) => void;
}) {
  const { org, project } = useProject();
  const layout = useListLayout();
  const queryClient = useQueryClient();
  const save = $api.useMutation("patch", "/api/v1/projects/{project}/datasets/{dataset}/items");
  const remove = $api.useMutation(
    "post",
    "/api/v1/projects/{project}/datasets/{dataset}/items/delete",
  );
  const [checked, setChecked] = useState<ReadonlySet<string>>(() => new Set());
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [editing, setEditing] = useState(false);
  const [drafts, setDrafts] = useState<Drafts>({});
  const [cellErrors, setCellErrors] = useState<CellErrors>({});
  const [editError, setEditError] = useState<string | undefined>();
  const [confirmingLeave, setConfirmingLeave] = useState(false);
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/datasets/{dataset}/items",
    datasetItemsInit(org, project, datasetId, version, search),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );

  if (query.error) {
    throw query.error;
  }

  const ref = { project: projectRef(org, project), dataset: datasetId };
  const rows = useMemo(() => query.data?.pages.flatMap((page) => page.data) ?? [], [query.data]);
  const openIndex = rows.findIndex((item) => item.id === openItemId);
  const openItem = openIndex >= 0 ? rows[openIndex] : undefined;
  const previous = openIndex > 0 ? rows[openIndex - 1] : undefined;
  const next = openIndex >= 0 ? rows[openIndex + 1] : undefined;
  const columns = useMemo(() => itemColumns(), []);
  const changes = changedFields(drafts);
  const changedItems = Object.keys(drafts).length;
  const cellEdit = useMemo<CellEdit | undefined>(
    () =>
      editing
        ? {
            value: (item, field) => draftedValue(item, drafts, field),
            changed: (item, field) => hasDraft(drafts, item.id, field),
            error: (id, field) => cellErrors[id]?.[field],
            apply: (item, field, value) => {
              setDrafts((current) => withDraft(current, item, field, value));
              setCellErrors((current) => {
                const cell = current[item.id];
                if (cell === undefined || cell[field] === undefined) {
                  return current;
                }
                const { [field]: _cleared, ...rest } = cell;
                return { ...current, [item.id]: rest };
              });
            },
          }
        : undefined,
    [editing, drafts, cellErrors],
  );

  const emptyState =
    search !== undefined ? (
      <EmptyState icon={Search} title="No item matches" />
    ) : !latest ? (
      <EmptyState icon={Table2} title="No items in this version" />
    ) : editable ? (
      <DatasetItemsEmpty onAddItem={onAddItem} onImport={onImport} />
    ) : (
      <EmptyState icon={Table2} title="No items" />
    );

  function invalidate() {
    for (const key of datasetWriteKeys(org, project, datasetId)) {
      void queryClient.invalidateQueries({ queryKey: key });
    }
  }

  function leaveEditing() {
    setEditing(false);
    setDrafts({});
    setCellErrors({});
    setEditError(undefined);
  }

  function toggleEditing() {
    if (!editing) {
      setEditing(true);
    } else if (changes > 0) {
      setConfirmingLeave(true);
    } else {
      leaveEditing();
    }
  }

  function onSave() {
    setEditError(undefined);
    const ids = Object.keys(drafts);
    save.mutate(
      { params: { path: ref }, body: editBody(drafts) },
      {
        onSuccess: () => {
          invalidate();
          leaveEditing();
        },
        onError: (caught) => {
          const { cells, other } = editErrorsOf(caught, ids);
          setCellErrors(cells);
          setEditError(
            other.length > 0
              ? other.join(" ")
              : Object.keys(cells).length === 0
                ? problemMessage(caught, FALLBACK_SAVE)
                : undefined,
          );
        },
      },
    );
  }

  function onDelete() {
    const ids = [...checked];
    remove.mutate(
      { params: { path: ref }, body: { ids } },
      {
        onSuccess: () => {
          invalidate();
          setChecked(new Set());
          setConfirmingDelete(false);
          setDrafts((current) => omitKeys(current, ids));
          setCellErrors((current) => omitKeys(current, ids));
          if (openItemId !== undefined && ids.includes(openItemId)) {
            onOpenItem(undefined);
          }
        },
      },
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-2">
      <div className="flex shrink-0 flex-wrap items-center gap-2">
        <ItemSearch value={search} onChange={onSearch} />
        {editable ? (
          <Button
            type="button"
            variant={editing ? "secondary" : "outline"}
            size="xs"
            aria-pressed={editing}
            onClick={toggleEditing}
          >
            Edit cells
          </Button>
        ) : null}
      </div>
      {checked.size > 0 ? (
        <SelectionBar
          count={checked.size}
          onDelete={() => {
            setConfirmingDelete(true);
          }}
          onClear={() => {
            setChecked(new Set());
          }}
        />
      ) : null}
      {editing ? (
        <EditBar
          changes={changes}
          items={changedItems}
          saving={save.isPending}
          error={editError}
          onDiscard={() => {
            setDrafts({});
            setCellErrors({});
            setEditError(undefined);
          }}
          onSave={onSave}
        />
      ) : null}
      <div className="relative flex min-h-0 flex-1 flex-col">
        <div className="absolute inset-0">
          {query.isPending ? (
            <div className="flex flex-col gap-2">
              <Skeleton className="h-7 w-full" />
              <Skeleton className="h-7 w-full" />
              <Skeleton className="h-7 w-full" />
            </div>
          ) : (
            <SplitPane
              name="dataset-items"
              mode={layout}
              open={openItem !== undefined}
              filmstrip={
                <Filmstrip
                  items={rows.map((item) => ({
                    id: item.id,
                    label: itemShortId(item.id),
                    detail: `v${item.version}`,
                    status: "ok",
                  }))}
                  selectedId={openItemId}
                  onSelect={(id) => {
                    onOpenItem(id);
                  }}
                  onBack={() => {
                    onOpenItem(undefined);
                  }}
                />
              }
              start={
                <ItemEditContext.Provider value={cellEdit}>
                  <div className="flex h-full min-h-0 flex-col">
                    <DataTable
                      tableId="dataset-items"
                      ariaLabel="Items"
                      columns={columns}
                      rows={rows}
                      getRowId={(item) => item.id}
                      selectedRowId={openItemId}
                      onActivate={(item) => {
                        onOpenItem(item.id);
                      }}
                      onSelectionMove={(item) => {
                        onOpenItem(item.id);
                      }}
                      hasNextPage={query.hasNextPage}
                      isFetchingNextPage={query.isFetchingNextPage}
                      fetchNextPage={() => {
                        void query.fetchNextPage();
                      }}
                      emptyState={emptyState}
                      checkedIds={checked}
                      onCheckedChange={editable ? setChecked : undefined}
                    />
                  </div>
                </ItemEditContext.Provider>
              }
              end={
                openItem ? (
                  <ItemPanel
                    key={openItem.id}
                    datasetId={datasetId}
                    item={openItem}
                    editable={editable}
                    previous={previous}
                    next={next}
                    onMove={(item) => {
                      onOpenItem(item.id);
                    }}
                    onClose={() => {
                      onOpenItem(undefined);
                    }}
                    onDirtyChange={onItemDirtyChange}
                    layout={<TraceLayoutControls />}
                  />
                ) : null
              }
            />
          )}
        </div>
      </div>
      <ConfirmDialog
        open={confirmingDelete}
        onOpenChange={(open) => {
          setConfirmingDelete(open);
          if (!open) {
            remove.reset();
          }
        }}
        title={`Delete ${checked.size} ${checked.size === 1 ? "item" : "items"}`}
        description="They leave the dataset in a new version. Older versions keep them."
        confirmLabel="Delete items"
        destructive
        pending={remove.isPending}
        error={remove.error ? problemMessage(remove.error, FALLBACK_DELETE) : undefined}
        onConfirm={onDelete}
      />
      <ConfirmDialog
        open={confirmingLeave}
        onOpenChange={setConfirmingLeave}
        title="Discard unsaved changes?"
        description={`${changes} unsaved ${changes === 1 ? "change" : "changes"} will be lost.`}
        confirmLabel="Discard changes"
        destructive
        pending={false}
        onConfirm={() => {
          setConfirmingLeave(false);
          leaveEditing();
        }}
      />
    </div>
  );
}
