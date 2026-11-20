import { useQuery } from "@tanstack/react-query";
import { promptSettingsQuery } from "@/api/queries-prompts";
import type { Schema } from "@/api/types";
import type { DataTableColumn } from "@/components/data-table/data-table";
import { LabelChip } from "@/components/prompts/label-chip";
import { RelativeTime } from "@/components/relative-time";
import { useProject } from "@/components/shell/project-context";

type Prompt = Schema<"Prompt">;

/** The labels of a prompt as chips, with a lock on the protected ones. */
function LabelsCell({ prompt }: { prompt: Prompt }) {
  const { org, project } = useProject();
  const settings = useQuery(promptSettingsQuery(org, project));
  const protectedLabels = settings.data?.protected_labels ?? [];
  if (prompt.labels.length === 0) {
    return <span className="text-muted-foreground">—</span>;
  }
  return (
    <div className="flex min-w-0 flex-wrap items-center gap-1">
      {prompt.labels.map((label) => (
        <LabelChip
          key={label.label}
          label={label.label}
          version={label.version}
          protectedLabel={protectedLabels.includes(label.label)}
        />
      ))}
    </div>
  );
}

/** The columns of the prompts list. Rows are not sorted on the client, so no column sorts. */
export const PROMPT_COLUMNS: DataTableColumn<Prompt>[] = [
  {
    id: "name",
    size: 240,
    header: "Name",
    enableSorting: false,
    meta: { grow: true },
    cell: ({ row }) => <span className="block truncate font-medium">{row.original.name}</span>,
  },
  {
    id: "type",
    size: 80,
    header: "Type",
    enableSorting: false,
    cell: ({ row }) => (
      <span className="text-muted-foreground">
        {row.original.type === "chat" ? "Chat" : "Text"}
      </span>
    ),
  },
  {
    id: "labels",
    size: 320,
    header: "Labels",
    enableSorting: false,
    cell: ({ row }) => <LabelsCell prompt={row.original} />,
  },
  {
    id: "latest",
    size: 80,
    header: "Latest",
    enableSorting: false,
    meta: { align: "right" },
    cell: ({ row }) => <span className="num">v{row.original.latest_version}</span>,
  },
  {
    id: "tags",
    size: 192,
    header: "Tags",
    enableSorting: false,
    cell: ({ row }) =>
      row.original.tags.length > 0 ? (
        <span className="block truncate text-muted-foreground">{row.original.tags.join(", ")}</span>
      ) : null,
  },
  {
    id: "updated",
    size: 120,
    header: "Updated",
    enableSorting: false,
    cell: ({ row }) => <RelativeTime value={row.original.updated_at} />,
  },
];
