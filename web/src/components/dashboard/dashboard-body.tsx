import { useQueryClient } from "@tanstack/react-query";
import { MoreHorizontal } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import type { Layout as GridLayout } from "react-grid-layout";
import { dashboardQuery, isStaleDashboard, useUpdateDashboard } from "@/api/dashboards";
import { problemMessage, problemOf } from "@/api/problem";
import { UnsavedChangesGuard } from "@/components/settings/unsaved-changes";
import { useProject } from "@/components/shell/project-context";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useIsMobile } from "@/hooks/use-mobile";
import {
  bottomOf,
  issueWidgetIndex,
  issueWithin,
  type Nudge,
  newWidgetId,
  nudgedLayout,
  sameDefinition,
  withLayouts,
} from "@/lib/dashboards/draft";
import { OVERVIEW } from "@/lib/dashboards/overview";
import { useDashboardStore } from "@/lib/dashboards/store";
import type { Dashboard, Definition, Layout, Widget } from "@/lib/dashboards/types";
import { type FieldIssue, validateDefinition } from "@/lib/dashboards/validate";
import { canManageDashboards } from "@/lib/roles";
import { DashboardMenu, DashboardSwitcher } from "./dashboard-header";
import type { WidgetContext } from "./widget";
import { HeadingDialog, WidgetEditor } from "./widget-editor";
import { compactWidgets, nudgeWidgets, WidgetGrid } from "./widget-grid";

/** A dashboard's widgets, the most a definition holds (spec 04 § Dashboards). */
const MAX_WIDGETS = 50;
/** A new chart's size: half the grid, seven rows. */
const NEW_CHART = { w: 6, h: 7 } as const;

/**
 * An edit session: a draft of the definition, started from the version it was read at. The
 * save names that version, so a change made elsewhere since then is a `409`, not an overwrite.
 */
type Session = {
  version: number;
  base: Definition;
  draft: Definition;
  /** Client or server rule failures, by path. */
  issues: FieldIssue[];
  /** A failed save that is not about a field. */
  message?: string;
  /** The save found a newer version. */
  conflict: boolean;
};

/**
 * The dialog that is open. `index` is the widget's place in the draft; for a new widget it is
 * where it will be appended.
 */
type Open =
  | { kind: "widget"; index: number; base: Widget }
  | { kind: "heading"; index?: number; title: string }
  | null;

/**
 * The grid of the open dashboard, with its header. Members on a custom dashboard can edit it:
 * the draft is kept here until it is saved or dropped. The Overview is read only.
 */
export function DashboardBody({
  dashboard,
  active,
  context,
  renderWidget,
}: {
  /** The custom dashboard, or undefined for the Overview. */
  dashboard: Dashboard | undefined;
  /** Whether the dashboard is the one on screen. A hidden dashboard drops its draft. */
  active: boolean;
  context: WidgetContext;
  /** A widget's content. `edit` is given in edit mode: no clicks, and the actions for the title bar. */
  renderWidget: (widget: Widget, edit?: { actions: ReactNode }) => ReactNode;
}) {
  const { org, project, data } = useProject();
  const queryClient = useQueryClient();
  const narrow = useIsMobile();
  const { store, local } = useDashboardStore(org, project);
  const update = useUpdateDashboard(org, project, dashboard?.id ?? "");
  const [session, setSession] = useState<Session | null>(null);
  const [open, setOpen] = useState<Open>(null);

  // A hidden dashboard keeps its state, so leaving it drops the draft: the effect runs when it
  // is shown again.
  useEffect(() => {
    if (active) {
      setSession(null);
      setOpen(null);
    }
  }, [active]);

  // A demo-org viewer edits the dashboards kept in this browser (D-C-17).
  const canEdit = dashboard !== undefined && (local || canManageDashboards(data.role)) && !narrow;
  const editing = canEdit && session !== null;
  const draft = editing ? session.draft : undefined;
  const shown: Definition = draft ?? dashboard?.definition ?? OVERVIEW;
  // A draft is kept while the grid is not shown (a narrow window), so it still guards the page.
  const dirty = session !== null && !sameDefinition(session.draft, session.base);

  function changeDraft(change: (current: Definition) => Definition) {
    setSession((current) =>
      current === null
        ? current
        : { ...current, draft: change(current.draft), issues: [], message: undefined },
    );
  }

  function applyLayout(layout: GridLayout) {
    setSession((current) => {
      if (current === null) {
        return current;
      }
      const widgets = withLayouts(current.draft.widgets, layout);
      if (widgets === current.draft.widgets) {
        return current;
      }
      return { ...current, draft: { widgets }, issues: [], message: undefined };
    });
  }

  function startEditing() {
    if (dashboard === undefined) {
      return;
    }
    // The grid packs the widgets upwards when it is shown, so the base is packed the same way:
    // the first drag then changes only what the user moved.
    const definition = { widgets: compactWidgets(dashboard.definition.widgets) };
    setSession({
      version: dashboard.version,
      base: definition,
      draft: definition,
      issues: [],
      conflict: false,
    });
  }

  function cancelEditing() {
    setSession(null);
    setOpen(null);
  }

  function applyWidget(widget: Widget, index: number) {
    changeDraft((current) => ({ widgets: putWidget(current.widgets, index, widget) }));
  }

  function nudgeWidget(index: number, step: Nudge) {
    changeDraft((current) => ({ widgets: nudgeWidgets(current.widgets, index, step) }));
  }

  function removeWidget(index: number) {
    changeDraft((current) => ({
      widgets: current.widgets.filter((_, at) => at !== index),
    }));
  }

  function duplicateWidget(index: number) {
    changeDraft((current) => {
      const source = current.widgets[index];
      if (source === undefined) {
        return current;
      }
      const copy: Widget = {
        ...source,
        id: newWidgetId(),
        layout: {
          x: source.layout.x,
          y: bottomOf(current.widgets),
          w: source.layout.w,
          h: source.layout.h,
        },
      };
      return { widgets: [...current.widgets, copy] };
    });
  }

  function applyHeading(title: string, index: number | undefined) {
    changeDraft((current) => {
      if (index === undefined) {
        const heading: Widget = {
          id: newWidgetId(),
          title,
          kind: "heading",
          layout: { x: 0, y: bottomOf(current.widgets), w: 12, h: 1 },
        };
        return { widgets: [...current.widgets, heading] };
      }
      const existing = current.widgets[index];
      if (existing === undefined) {
        return current;
      }
      return { widgets: putWidget(current.widgets, index, { ...existing, title }) };
    });
  }

  function openNewWidget() {
    if (session === null) {
      return;
    }
    const index = session.draft.widgets.length;
    setOpen({
      kind: "widget",
      index,
      base: {
        id: newWidgetId(),
        title: "",
        kind: "chart",
        layout: { x: 0, y: bottomOf(session.draft.widgets), ...NEW_CHART },
      },
    });
  }

  function openWidget(index: number) {
    const widget = session?.draft.widgets[index];
    if (widget === undefined) {
      return;
    }
    setOpen(
      widget.kind === "heading"
        ? { kind: "heading", index, title: widget.title }
        : { kind: "widget", index, base: widget },
    );
  }

  async function save() {
    if (session === null || dashboard === undefined) {
      return;
    }
    const issues = validateDefinition(session.draft);
    if (issues.length > 0) {
      setSession({ ...session, issues, message: undefined });
      return;
    }
    try {
      await update.mutateAsync({ version: session.version, definition: session.draft });
      setSession(null);
    } catch (error) {
      const problem = problemOf(error);
      setSession((current) => {
        if (current === null) {
          return current;
        }
        if (isStaleDashboard(error)) {
          return { ...current, conflict: true };
        }
        if (problem?.status === 400 && (problem.errors?.length ?? 0) > 0) {
          return { ...current, issues: problem.errors ?? [], message: undefined };
        }
        return { ...current, message: problemMessage(error, "Could not save the dashboard.") };
      });
    }
  }

  function reload() {
    setSession(null);
    setOpen(null);
    if (dashboard !== undefined) {
      void queryClient.invalidateQueries({
        queryKey: dashboardQuery(org, project, dashboard.id, store).queryKey,
      });
    }
  }

  // Issues about a widget sit on its cell; the others, and a failed save, sit at the top.
  const issues = session?.issues ?? [];
  const topIssues = issues.filter((issue) => {
    const index = issueWidgetIndex(issue.field);
    return index === undefined || index >= shown.widgets.length;
  });
  const full = (draft?.widgets.length ?? 0) >= MAX_WIDGETS;
  // A save in flight locks the grid and its controls until the answer comes.
  const busy = update.isPending;
  const movable = editing && !busy;
  // The number a widget without a title is named by: its place in reading order, from 1.
  const places = new Map(
    [...shown.widgets]
      .sort((left, right) => left.layout.y - right.layout.y || left.layout.x - right.layout.x)
      .map((widget, at) => [widget.id, at + 1]),
  );

  return (
    <>
      <div className="flex flex-wrap items-center gap-2 pb-3">
        <DashboardSwitcher dashboard={dashboard} />
        <div className="ml-auto flex flex-wrap items-center gap-2">
          {editing ? (
            <>
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={full || busy}
                title={full ? `A dashboard holds at most ${MAX_WIDGETS} widgets.` : undefined}
                onClick={openNewWidget}
              >
                Add widget
              </Button>
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={full || busy}
                title={full ? `A dashboard holds at most ${MAX_WIDGETS} widgets.` : undefined}
                onClick={() => setOpen({ kind: "heading", title: "" })}
              >
                Add heading
              </Button>
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={busy}
                onClick={cancelEditing}
              >
                Cancel
              </Button>
              <Button type="button" size="sm" disabled={busy} onClick={() => void save()}>
                Save
              </Button>
            </>
          ) : canEdit ? (
            <Button type="button" variant="outline" size="sm" onClick={startEditing}>
              Edit
            </Button>
          ) : null}
          {editing ? null : <DashboardMenu dashboard={dashboard} />}
        </div>
      </div>

      {editing && local ? (
        <p role="status" className="pb-3 text-caption text-muted-foreground">
          These dashboards are stored in this browser only.
        </p>
      ) : null}

      {editing && (topIssues.length > 0 || session.message) ? (
        <div role="alert" className="flex flex-col gap-1 pb-3 text-caption text-destructive">
          {session.message ? <p>{session.message}</p> : null}
          {topIssues.map((issue) => (
            <p key={`${issue.field}:${issue.message}`}>
              {issueWithin(issue.field)}: {issue.message}
            </p>
          ))}
        </div>
      ) : null}

      <WidgetGrid
        widgets={shown.widgets}
        editing={movable}
        onLayoutChange={movable ? applyLayout : undefined}
        renderCell={(widget) => {
          if (!editing) {
            return renderWidget(widget);
          }
          const index = shown.widgets.findIndex((item) => item.id === widget.id);
          const own = issues.filter((issue) => issueWidgetIndex(issue.field) === index);
          const named = widget.title.trim() !== "";
          return (
            <>
              {renderWidget(widget, {
                actions: (
                  <WidgetMenu
                    label={
                      named
                        ? `Actions for ${widget.title}`
                        : `Actions for widget ${places.get(widget.id) ?? index + 1}`
                    }
                    layout={widget.layout}
                    disabled={busy}
                    full={full}
                    onEdit={() => openWidget(index)}
                    onDuplicate={() => duplicateWidget(index)}
                    onNudge={(step) => nudgeWidget(index, step)}
                    onRemove={() => removeWidget(index)}
                  />
                ),
              })}
              {own.length > 0 ? (
                <ul
                  role="alert"
                  className="absolute inset-x-1 bottom-1 z-10 flex flex-col gap-0.5 rounded-md bg-background/95 px-2 py-1 text-caption text-destructive shadow-xs"
                >
                  {own.map((issue) => (
                    <li key={`${issue.field}:${issue.message}`}>
                      {issueWithin(issue.field)}: {issue.message}
                    </li>
                  ))}
                </ul>
              ) : null}
            </>
          );
        }}
      />

      {draft !== undefined && open?.kind === "widget" ? (
        <WidgetEditor
          open
          onOpenChange={(next) => {
            if (!next) setOpen(null);
          }}
          base={open.base}
          draft={draft}
          index={open.index}
          context={context}
          onApply={(widget) => {
            applyWidget(widget, open.index);
            setOpen(null);
          }}
        />
      ) : null}
      {open?.kind === "heading" ? (
        <HeadingDialog
          open
          onOpenChange={(next) => {
            if (!next) setOpen(null);
          }}
          initial={open.title}
          onApply={(title) => {
            applyHeading(title, open.index);
            setOpen(null);
          }}
        />
      ) : null}

      <AlertDialog
        open={session?.conflict === true}
        onOpenChange={(next) => {
          if (!next) {
            setSession((current) => (current === null ? current : { ...current, conflict: false }));
          }
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Someone else changed this dashboard.</AlertDialogTitle>
            <AlertDialogDescription>
              Reload to see their version. Your edits are dropped.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Keep editing</AlertDialogCancel>
            <AlertDialogAction variant="destructive" onClick={reload}>
              Reload
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <UnsavedChangesGuard dirty={dirty} ignoreSearchChanges />
    </>
  );
}

/** The widgets with `widget` at `index`, or appended when `index` is past the end. */
function putWidget(widgets: Widget[], index: number, widget: Widget): Widget[] {
  return index < widgets.length
    ? widgets.map((item, at) => (at === index ? widget : item))
    : [...widgets, widget];
}

/** The menu of a widget in edit mode, in its title bar: edit, duplicate, move, resize, remove. */
function WidgetMenu({
  label,
  layout,
  disabled,
  full,
  onEdit,
  onDuplicate,
  onNudge,
  onRemove,
}: {
  label: string;
  layout: Layout;
  disabled: boolean;
  /** The dashboard holds the most widgets, so a copy is refused. */
  full: boolean;
  onEdit: () => void;
  onDuplicate: () => void;
  onNudge: (step: Nudge) => void;
  onRemove: () => void;
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          type="button"
          variant="outline"
          size="icon-xs"
          aria-label={label}
          disabled={disabled}
        >
          <MoreHorizontal />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="min-w-40">
        <DropdownMenuItem onSelect={onEdit}>Edit</DropdownMenuItem>
        <DropdownMenuItem disabled={full} onSelect={onDuplicate}>
          Duplicate
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        {NUDGES.map(([step, text]) => (
          <DropdownMenuItem
            key={step}
            disabled={nudgedLayout(layout, step) === undefined}
            onSelect={() => onNudge(step)}
          >
            {text}
          </DropdownMenuItem>
        ))}
        <DropdownMenuSeparator />
        <DropdownMenuItem variant="destructive" onSelect={onRemove}>
          Remove
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/** The menu's steps, in the order it lists them. */
const NUDGES: readonly [Nudge, string][] = [
  ["up", "Move up"],
  ["down", "Move down"],
  ["left", "Move left"],
  ["right", "Move right"],
  ["wider", "Wider"],
  ["narrower", "Narrower"],
  ["taller", "Taller"],
  ["shorter", "Shorter"],
];
