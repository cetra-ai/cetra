import { useNavigate } from "@tanstack/react-router";
import { Plus } from "lucide-react";
import type { ReactNode } from "react";
import { useProject } from "@/components/shell/project-context";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { type Field, serializeChip } from "@/lib/dsl/dsl";

/** The page a cell's id opens from the context menu. */
export type OpenTarget = "session" | "user";

/**
 * A cell value that filters the list on click (hover "+") and from its context menu.
 * Scalar fields get `field = 'value'` and `field != 'value'`; a list field (`list`) gets
 * `field @> ['value']` and no exclude. `opens` adds an "Open ..." item for an id.
 */
export function FilterHit({
  label,
  field,
  value,
  list = false,
  opens,
  onFilter,
  children,
}: {
  label: string;
  field: Field;
  value: string;
  list?: boolean;
  opens?: OpenTarget;
  onFilter?: (conjunct: string) => void;
  children: ReactNode;
}) {
  const include = list
    ? serializeChip({ kind: "contains", field, values: [value] })
    : serializeChip({ kind: "compare", field, op: "=", value: { kind: "string", value } });
  const exclude = serializeChip({
    kind: "compare",
    field,
    op: "!=",
    value: { kind: "string", value },
  });
  return (
    <ContextMenu>
      <ContextMenuTrigger className="group flex min-w-0 items-center gap-1">
        <span className="min-w-0 truncate">{children}</span>
        {onFilter ? (
          <button
            type="button"
            aria-label={label}
            className="inline-flex size-4 shrink-0 items-center justify-center text-muted-foreground opacity-0 group-hover:opacity-100 focus-visible:opacity-100"
            onClick={(event) => {
              event.stopPropagation();
              onFilter(include);
            }}
          >
            <Plus className="size-3" />
          </button>
        ) : null}
      </ContextMenuTrigger>
      {/* The menu is portalled, but React events still bubble to the table row: stop them here. */}
      <ContextMenuContent
        onClick={(event) => event.stopPropagation()}
        onKeyDown={(event) => event.stopPropagation()}
        onPointerDown={(event) => event.stopPropagation()}
      >
        {onFilter ? (
          <>
            <MenuLabelItem onSelect={() => onFilter(include)}>
              {list ? `Filter: ${field.column} contains ${value}` : `Filter: ${include}`}
            </MenuLabelItem>
            {list ? null : (
              <MenuLabelItem onSelect={() => onFilter(exclude)}>
                {`Exclude: ${exclude}`}
              </MenuLabelItem>
            )}
            <ContextMenuSeparator />
          </>
        ) : null}
        <ContextMenuItem
          onSelect={() => {
            void navigator.clipboard.writeText(value).catch(() => undefined);
          }}
        >
          Copy value
        </ContextMenuItem>
        {opens ? <OpenItem target={opens} id={value} /> : null}
      </ContextMenuContent>
    </ContextMenu>
  );
}

/** A menu item whose text may be long: truncated, with the full text in the tooltip. */
function MenuLabelItem({ onSelect, children }: { onSelect: () => void; children: string }) {
  return (
    <ContextMenuItem className="max-w-80 truncate" title={children} onSelect={onSelect}>
      {children}
    </ContextMenuItem>
  );
}

/** Mounted only while the menu is open, so cells render without a router or project context. */
function OpenItem({ target, id }: { target: OpenTarget; id: string }) {
  const { org, project } = useProject();
  const navigate = useNavigate();
  return (
    <ContextMenuItem
      onSelect={() => {
        if (target === "session") {
          void navigate({
            to: "/$org/$project/sessions/$sessionId",
            params: { org, project, sessionId: id },
          });
        } else {
          void navigate({
            to: "/$org/$project/users/$userId",
            params: { org, project, userId: id },
          });
        }
      }}
    >
      {`Open ${target}`}
    </ContextMenuItem>
  );
}

/** An id cell (session, user): its value filters the list and opens its page from the menu. */
export function filterIdCell(
  value: string | undefined,
  label: string,
  column: string,
  opens: OpenTarget,
  onFilter?: (conjunct: string) => void,
): ReactNode {
  if (!value) {
    return "—";
  }
  return (
    <FilterHit
      label={`Filter by ${label} ${value}`}
      field={{ column }}
      value={value}
      opens={opens}
      onFilter={onFilter}
    >
      {value}
    </FilterHit>
  );
}
