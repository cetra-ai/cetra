import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { cn } from "cn";
import { FileText, Hash, Layers, Tags, Waypoints } from "lucide-react";
import { createContext, type ReactNode, type SyntheticEvent, useContext, useState } from "react";
import { problemOf } from "@/api/problem";
import { traceDetailQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import type { DataTableColumn } from "@/components/data-table/data-table";
import { type ItemField, JsonField } from "@/components/datasets/item-fields";
import { LazyTooltip } from "@/components/lazy-tooltip";
import { RelativeTime } from "@/components/relative-time";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Skeleton } from "@/components/ui/skeleton";
import { itemShortId, parseJsonField, prettyJson, previewFields } from "@/lib/datasets";
import { formatDuration } from "@/lib/format";

/** How many characters of the pretty JSON a hover shows. */
const TIP_CHARS = 2000;

type Item = Schema<"DatasetItem">;

/**
 * The edit state the items cells read in edit mode. A cell shows its draft when it has one, and
 * `apply` sets a cell's draft from its editor. Absent outside edit mode.
 */
export interface CellEdit {
  value: (item: Item, field: ItemField) => unknown;
  changed: (item: Item, field: ItemField) => boolean;
  /** The field error of the cell from the last save. */
  error: (itemId: string, field: ItemField) => string | undefined;
  apply: (item: Item, field: ItemField, value: unknown) => void;
}

/**
 * Read by the editable cells through a context rather than through the columns, so the memoised
 * rows re-render when the drafts change.
 */
export const ItemEditContext = createContext<CellEdit | undefined>(undefined);

/** A value's one-line preview, with its pretty JSON (cut) on hover. */
function PreviewCell({ value }: { value: unknown }) {
  const tip = value === undefined ? "—" : prettyJson(value).slice(0, TIP_CHARS);
  return (
    <LazyTooltip
      content={
        <pre className="max-h-80 max-w-md overflow-auto font-mono text-caption whitespace-pre-wrap">
          {tip}
        </pre>
      }
    >
      <span className="block min-w-0 truncate font-mono text-body">{previewFields(value)}</span>
    </LazyTooltip>
  );
}

/** Stops a click or key press in an editor from reaching the row, which would open the item. */
function stop(event: SyntheticEvent) {
  event.stopPropagation();
}

/**
 * A value cell. Outside edit mode it shows `view`. In edit mode it is a button with its one-line
 * value (or the field error), and a popover with the JSON editor.
 */
function EditableCell({
  item,
  field,
  label,
  view,
}: {
  item: Item;
  field: ItemField;
  label: string;
  view: ReactNode;
}) {
  const edit = useContext(ItemEditContext);
  if (!edit) {
    return view;
  }
  return <EditCellButton edit={edit} item={item} field={field} label={label} />;
}

function EditCellButton({
  edit,
  item,
  field,
  label,
}: {
  edit: CellEdit;
  item: Item;
  field: ItemField;
  label: string;
}) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState("");
  const [problem, setProblem] = useState<string | undefined>();
  const value = edit.value(item, field);
  const error = edit.error(item.id, field);
  const changed = edit.changed(item, field);

  function onOpenChange(next: boolean) {
    if (next) {
      // An absent or cleared expected output starts empty, not as "null".
      setText(value === undefined || value === null ? "" : prettyJson(value));
      setProblem(undefined);
    }
    setOpen(next);
  }

  function apply() {
    const parsed = parseJsonField(text, {
      required: field === "input",
      object: field === "metadata",
    });
    if (!parsed.ok) {
      setProblem(parsed.message);
      return;
    }
    // An empty expected output clears it (null); empty metadata is an empty object.
    const next = parsed.value ?? (field === "metadata" ? {} : null);
    edit.apply(item, field, next);
    setOpen(false);
  }

  return (
    <Popover open={open} onOpenChange={onOpenChange}>
      <PopoverTrigger asChild>
        <button
          type="button"
          aria-label={`Edit ${label} of item ${itemShortId(item.id)}`}
          title={error}
          onClick={stop}
          className={cn(
            "block w-full min-w-0 truncate rounded-sm px-0.5 text-left font-mono text-body outline-none focus-visible:outline-2 focus-visible:outline-ring",
            changed && "bg-primary/10",
            error !== undefined && "text-destructive",
          )}
        >
          {error ?? previewFields(value)}
        </button>
      </PopoverTrigger>
      <PopoverContent
        align="start"
        className="flex w-96 flex-col gap-2 p-3 text-body"
        onClick={stop}
        onKeyDown={stop}
      >
        <JsonField
          label={label}
          value={text}
          rows={8}
          error={problem}
          onChange={(next) => {
            setText(next);
            setProblem(undefined);
          }}
        />
        <div className="flex justify-end gap-2">
          <Button
            type="button"
            variant="outline"
            size="xs"
            onClick={() => {
              setOpen(false);
            }}
          >
            Cancel
          </Button>
          <Button type="button" size="xs" onClick={apply}>
            Apply
          </Button>
        </div>
      </PopoverContent>
    </Popover>
  );
}

/** The columns of the items table. Previews are one line; the full JSON is in the hover. */
export function itemColumns(): DataTableColumn<Item>[] {
  return [
    {
      id: "item",
      size: 96,
      header: "Item",
      enableSorting: false,
      meta: { mono: true, icon: Hash },
      cell: ({ row }) => (
        <span className="font-mono text-body">{itemShortId(row.original.id)}</span>
      ),
    },
    {
      id: "input",
      size: 240,
      header: "Input",
      enableSorting: false,
      meta: { grow: true, icon: FileText },
      cell: ({ row }) => (
        <EditableCell
          item={row.original}
          field="input"
          label="Input"
          view={<PreviewCell value={row.original.input} />}
        />
      ),
    },
    {
      id: "expected",
      size: 200,
      header: "Expected output",
      enableSorting: false,
      meta: { icon: FileText },
      cell: ({ row }) => (
        <EditableCell
          item={row.original}
          field="expected_output"
          label="Expected output"
          view={<PreviewCell value={row.original.expected_output} />}
        />
      ),
    },
    {
      id: "metadata",
      size: 160,
      header: "Metadata",
      enableSorting: false,
      meta: { icon: Tags },
      cell: ({ row }) => (
        <EditableCell
          item={row.original}
          field="metadata"
          label="Metadata"
          view={
            Object.keys(row.original.metadata).length === 0 ? (
              <span className="text-muted-foreground">—</span>
            ) : (
              <PreviewCell value={row.original.metadata} />
            )
          }
        />
      ),
    },
    {
      id: "source",
      size: 120,
      header: "Source",
      enableSorting: false,
      meta: { icon: Waypoints },
      cell: ({ row }) => <SourceCell item={row.original} />,
    },
    {
      id: "version",
      size: 64,
      header: "Added in",
      enableSorting: false,
      meta: { align: "right", mono: true, icon: Layers },
      cell: ({ row }) => <span className="num">{`v${row.original.version}`}</span>,
    },
    {
      id: "created",
      size: 96,
      header: "Created",
      enableSorting: false,
      meta: { align: "right", icon: Layers },
      cell: ({ row }) => (
        <span className="num text-muted-foreground">
          <RelativeTime value={row.original.created_at} />
        </span>
      ),
    },
  ];
}

/** The item's source trace as a link, with the trace's details on hover. "—" when it has none. */
function SourceCell({ item }: { item: Item }) {
  if (item.source_trace_id === undefined) {
    return <span className="text-muted-foreground">—</span>;
  }
  return <SourceLink traceId={item.source_trace_id} spanId={item.source_span_id} />;
}

/** A link to the source trace (and span), with the trace's name, start and duration on hover. */
export function SourceLink({ traceId, spanId }: { traceId: string; spanId?: string }) {
  const { org, project } = useProject();
  // The tooltip wraps a plain span: a router link does not pass the trigger's pointer handler on.
  return (
    <LazyTooltip content={<TraceTip traceId={traceId} />}>
      <span className="inline-flex">
        <Link
          to="/$org/$project/traces/$traceId"
          params={{ org, project, traceId }}
          search={spanId !== undefined ? { span: spanId } : {}}
          className="font-mono text-body underline-offset-4 hover:underline"
        >
          {`trace ${traceId.slice(0, 8)}`}
        </Link>
      </span>
    </LazyTooltip>
  );
}

/** The source trace's summary, fetched when the tooltip opens. */
function TraceTip({ traceId }: { traceId: string }) {
  const { org, project } = useProject();
  const query = useQuery(traceDetailQuery(org, project, traceId));
  if (query.isPending) {
    return <Skeleton className="h-8 w-40" />;
  }
  if (query.isError) {
    return problemOf(query.error)?.status === 404 ? (
      <span>Trace no longer available</span>
    ) : (
      <span>Could not load the trace</span>
    );
  }
  const trace = query.data.trace;
  return (
    <dl className="grid grid-cols-[auto_1fr] gap-x-2 gap-y-0.5 text-body">
      <dt className="opacity-70">Name</dt>
      <dd className="truncate">{trace.name}</dd>
      <dt className="opacity-70">Started</dt>
      <dd>
        <TimeText value={trace.start_time} style="datetime" />
      </dd>
      <dt className="opacity-70">Duration</dt>
      <dd className="num">{formatDuration(trace.duration_ns)}</dd>
    </dl>
  );
}
